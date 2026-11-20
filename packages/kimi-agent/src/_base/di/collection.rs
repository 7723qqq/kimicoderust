use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::dependency_graph::ObjectId;
use super::descriptors::DynamicValue;
use super::instantiation::{
    DependencyKind, DiTargetObj, ServiceIdentifier, refine_service_decorator,
    create_decorator, store_custom_dependency,
};
use crate::_base::event::{Emitter, Event};
use crate::_base::lifecycle::ledger::Ledger;

pub type CollectionValidator = Arc<
    dyn Fn(DynamicValue, Vec<DynamicValue>) -> Result<(), String> + Send + Sync,
>;

struct ErasedCollectionToken {
    service: ServiceIdentifier<DynamicValue>,
    name: String,
    definition: bool,
    validator: Option<CollectionValidator>,
}

static COLLECTION_TOKENS: Lazy<Mutex<HashMap<String, ErasedCollectionToken>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub struct CollectionToken<T = DynamicValue> {
    pub name: String,
    service: ServiceIdentifier<DynamicValue>,
    definition: bool,
    validator: Option<CollectionValidator>,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for CollectionToken<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            service: self.service.clone(),
            definition: self.definition,
            validator: self.validator.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Display for CollectionToken<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "collection:{}", self.name)
    }
}

impl<T> std::fmt::Debug for CollectionToken<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, formatter)
    }
}

impl<T> PartialEq for CollectionToken<T> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<T> Eq for CollectionToken<T> {}

impl<T> std::hash::Hash for CollectionToken<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.name, state);
    }
}

impl<T> CollectionToken<T> {
    pub fn decorate(&self, target: &mut DiTargetObj, index: usize) {
        store_custom_dependency(
            &self.service,
            DependencyKind::Collection,
            target,
            index,
        );
    }

    pub fn service_identifier(&self) -> ServiceIdentifier<T> {
        refine_service_decorator(&self.service)
    }

    fn erased(&self) -> CollectionToken<DynamicValue> {
        CollectionToken {
            name: self.name.clone(),
            service: self.service.clone(),
            definition: self.definition,
            validator: self.validator.clone(),
            marker: PhantomData,
        }
    }
}

pub type DefinitionToken<T> = CollectionToken<T>;
pub type CollectionDescriptor<T> = CollectionToken<T>;
pub type CollectionView<T> = Arc<CollectionViewImpl<T>>;

pub struct CollectionOptions<T> {
    pub validate: Option<Arc<dyn Fn(&T, &[T]) -> Result<(), String> + Send + Sync>>,
}

impl<T> Default for CollectionOptions<T> {
    fn default() -> Self {
        Self { validate: None }
    }
}

impl<T> CollectionOptions<T> {
    pub fn validating(
        validate: impl Fn(&T, &[T]) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            validate: Some(Arc::new(validate)),
        }
    }
}

pub fn collection<T>(
    name: impl Into<String>,
    options: CollectionOptions<T>,
) -> CollectionToken<T>
where
    T: Any + Clone + Send + Sync,
{
    let name = name.into();
    let mut tokens = COLLECTION_TOKENS.lock();
    if let Some(existing) = tokens.get(&name) {
        return CollectionToken {
            name: existing.name.clone(),
            service: existing.service.clone(),
            definition: existing.definition,
            validator: existing.validator.clone(),
            marker: PhantomData,
        };
    }
    let validator = options.validate.map(|validate| {
        Arc::new(move |value: DynamicValue, existing: Vec<DynamicValue>| {
            let value = Arc::downcast::<T>(value)
                .map_err(|_| "collection value type does not match its token".to_string())?;
            let existing = existing
                .into_iter()
                .map(|value| {
                    Arc::downcast::<T>(value)
                        .map(|value| value.as_ref().clone())
                        .map_err(|_| "collection value type does not match its token".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            validate(value.as_ref(), &existing)
        }) as CollectionValidator
    });
    let service = create_decorator(format!("collection:{name}"));
    tokens.insert(
        name.clone(),
        ErasedCollectionToken {
            service: service.clone(),
            name: name.clone(),
            definition: false,
            validator: validator.clone(),
        },
    );
    CollectionToken {
        name,
        service,
        definition: false,
        validator,
        marker: PhantomData,
    }
}

pub fn definition<T>(name: impl Into<String>) -> DefinitionToken<T>
where
    T: Any + Clone + Send + Sync,
{
    let name = name.into();
    let token = collection::<T>(name.clone(), CollectionOptions::default());
    let mut tokens = COLLECTION_TOKENS.lock();
    if let Some(existing) = tokens.get_mut(&name) {
        if existing.validator.is_none() {
            let validation_name = name.clone();
            existing.validator = Some(Arc::new(move |_, existing: Vec<DynamicValue>| {
                if existing.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "Definition {validation_name} already has an active provider"
                    ))
                }
            }));
        }
        existing.definition = true;
        return CollectionToken {
            name: existing.name.clone(),
            service: existing.service.clone(),
            definition: true,
            validator: existing.validator.clone(),
            marker: PhantomData,
        };
    }
    drop(tokens);
    token
}

pub fn is_collection_token<T>(token: &CollectionToken<T>) -> bool {
    COLLECTION_TOKENS.lock().contains_key(&token.name)
}

pub fn is_definition_token<T>(token: &CollectionToken<T>) -> bool {
    token.definition
}

#[derive(Clone)]
pub struct DefinitionRecord<T> {
    pub definition: T,
    pub owner: String,
    pub generation: u64,
}

#[derive(Clone)]
pub struct DefinitionChange<T> {
    pub current: Option<DefinitionRecord<T>>,
    pub previous: Option<DefinitionRecord<T>>,
}

#[derive(Clone)]
pub struct CollectionRecord<T> {
    pub value: T,
    pub provider_name: String,
    pub scope_path: String,
}

#[derive(Clone)]
pub struct CollectionChange<T> {
    pub added: Vec<T>,
    pub removed: Vec<T>,
}

#[derive(Clone)]
pub struct StoredRecord {
    pub id: u64,
    pub value: DynamicValue,
    pub provider_name: String,
    pub scope_path: String,
    pub provider: ObjectId,
    pub provider_book: Ledger,
}

trait ViewDispatch: Send + Sync {
    fn view_id(&self) -> ObjectId;
    fn consumer(&self) -> ObjectId;
    fn token_name(&self) -> &str;
    fn fire_delta(&self, added: bool, records: &[StoredRecord]);
}

pub struct CollectionStore {
    records: Mutex<HashMap<String, IndexMap<u64, StoredRecord>>>,
    views: Mutex<Vec<Arc<dyn ViewDispatch>>>,
    next_id: Mutex<u64>,
    parent_of: Arc<dyn Fn(ObjectId) -> Option<ObjectId> + Send + Sync>,
}

impl CollectionStore {
    pub fn new(
        parent_of: impl Fn(ObjectId) -> Option<ObjectId> + Send + Sync + 'static,
    ) -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            views: Mutex::new(Vec::new()),
            next_id: Mutex::new(0),
            parent_of: Arc::new(parent_of),
        }
    }

    pub fn add_record<T>(
        self: &Arc<Self>,
        token: &CollectionToken<T>,
        provider: ObjectId,
        provider_name: impl Into<String>,
        scope_path: impl Into<String>,
        provider_book: Ledger,
        value: T,
    ) -> Result<Box<dyn FnOnce() + Send>, String>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        let value = Arc::new(value) as DynamicValue;
        let existing = self
            .records
            .lock()
            .get(&token.name)
            .map(|records| records.values().map(|entry| entry.value.clone()).collect())
            .unwrap_or_default();
        if let Some(validate) = &token.validator {
            validate(value.clone(), existing)?;
        }
        let mut next_id = self.next_id.lock();
        *next_id += 1;
        let record = StoredRecord {
            id: *next_id,
            value,
            provider_name: provider_name.into(),
            scope_path: scope_path.into(),
            provider,
            provider_book,
        };
        drop(next_id);
        self.records
            .lock()
            .entry(token.name.clone())
            .or_default()
            .insert(record.id, record.clone());
        self.fire(token, provider, true, &[record.clone()]);
        let store = self.clone();
        let token_name = token.name.clone();
        let erased = token.erased();
        let id = record.id;
        Ok(Box::new(move || {
            let removed = store
                .records
                .lock()
                .get_mut(&token_name)
                .and_then(|records| records.shift_remove(&id));
            if let Some(record) = removed {
                store.fire(&erased, provider, false, &[record]);
            }
        }))
    }

    fn fire<T>(
        &self,
        token: &CollectionToken<T>,
        provider: ObjectId,
        added: bool,
        records: &[StoredRecord],
    ) {
        let views = self
            .views
            .lock()
            .iter()
            .filter(|view| view.token_name() == token.name)
            .cloned()
            .collect::<Vec<_>>();
        for view in views {
            let consumer = view.consumer();
            if consumer == provider || self.is_related(consumer, provider) {
                view.fire_delta(added, records);
            }
        }
    }

    pub fn create_view<T>(
        self: &Arc<Self>,
        token: CollectionToken<T>,
        consumer: ObjectId,
    ) -> Arc<CollectionViewImpl<T>>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        let view = Arc::new(CollectionViewImpl::new(self.clone(), token, consumer));
        self.views.lock().push(view.clone() as Arc<dyn ViewDispatch>);
        view
    }

    pub fn drop_view(&self, view_id: ObjectId) {
        self.views
            .lock()
            .retain(|view| view.view_id() != view_id);
    }

    pub fn records_for<T>(
        &self,
        token: &CollectionToken<T>,
        consumer: ObjectId,
    ) -> Vec<CollectionRecord<T>>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        self.stored_records_for(&token.erased(), consumer)
            .into_iter()
            .map(|record| CollectionRecord {
                value: downcast_value::<T>(&record.value),
                provider_name: record.provider_name,
                scope_path: record.scope_path,
            })
            .collect()
    }

    pub fn stored_records_for(
        &self,
        token: &CollectionToken<DynamicValue>,
        consumer: ObjectId,
    ) -> Vec<StoredRecord> {
        let records = self.records.lock();
        records
            .get(&token.name)
            .map(|records| {
                records
                    .values()
                    .filter(|record| {
                        record.provider == consumer
                            || self.is_related(consumer, record.provider)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn definition_for<T>(
        &self,
        token: &CollectionToken<T>,
        consumer: ObjectId,
    ) -> Option<DefinitionRecord<T>>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        let record = self.stored_records_for(&token.erased(), consumer).into_iter().next()?;
        Some(DefinitionRecord {
            definition: downcast_value::<T>(&record.value),
            owner: format!("{}@{}", record.provider_name, record.scope_path),
            generation: record.id,
        })
    }

    fn is_related(&self, consumer: ObjectId, provider: ObjectId) -> bool {
        let mut current = Some(consumer);
        while let Some(scope) = current {
            if scope == provider {
                return true;
            }
            current = (self.parent_of)(scope);
        }
        let mut current = Some(provider);
        while let Some(scope) = current {
            if scope == consumer {
                return true;
            }
            current = (self.parent_of)(scope);
        }
        false
    }
}

static NEXT_VIEW_ID: AtomicUsize = AtomicUsize::new(1);

pub struct CollectionViewImpl<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    store: Arc<CollectionStore>,
    id: ObjectId,
    pub token: CollectionToken<T>,
    pub consumer: ObjectId,
    on_did_change: Emitter<CollectionChange<T>>,
    on_did_change_definition: Emitter<DefinitionChange<T>>,
    pub on_did_change_event: Event<CollectionChange<T>>,
    pub on_did_change_definition_event: Event<DefinitionChange<T>>,
}

impl<T> CollectionViewImpl<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    fn new(store: Arc<CollectionStore>, token: CollectionToken<T>, consumer: ObjectId) -> Self {
        let on_did_change = Emitter::new();
        let on_did_change_definition = Emitter::new();
        Self {
            store,
            id: NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed),
            token,
            consumer,
            on_did_change_event: on_did_change.event(),
            on_did_change_definition_event: on_did_change_definition.event(),
            on_did_change,
            on_did_change_definition,
        }
    }

    pub fn records(&self) -> Vec<CollectionRecord<T>> {
        self.store.records_for(&self.token, self.consumer)
    }

    pub fn items(&self) -> Vec<T> {
        self.records().into_iter().map(|record| record.value).collect()
    }

    pub fn current(&self) -> Option<DefinitionRecord<T>> {
        self.store.definition_for(&self.token, self.consumer)
    }

    pub fn dispose(&self) {
        self.store.drop_view(self.id);
        self.on_did_change.dispose();
        self.on_did_change_definition.dispose();
    }

    fn definition_record(&self, record: Option<&StoredRecord>) -> Option<DefinitionRecord<T>> {
        record.map(|record| DefinitionRecord {
            definition: downcast_value::<T>(&record.value),
            owner: format!("{}@{}", record.provider_name, record.scope_path),
            generation: record.id,
        })
    }
}

impl<T> ViewDispatch for CollectionViewImpl<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    fn view_id(&self) -> ObjectId {
        self.id
    }

    fn consumer(&self) -> ObjectId {
        self.consumer
    }

    fn token_name(&self) -> &str {
        &self.token.name
    }

    fn fire_delta(&self, added: bool, records: &[StoredRecord]) {
        let previous = if added {
            None
        } else {
            self.definition_record(records.first())
        };
        let values = records
            .iter()
            .map(|record| downcast_value::<T>(&record.value))
            .collect::<Vec<_>>();
        self.on_did_change.fire(if added {
            CollectionChange {
                added: values,
                removed: Vec::new(),
            }
        } else {
            CollectionChange {
                added: Vec::new(),
                removed: values,
            }
        });
        if is_definition_token(&self.token) {
            self.on_did_change_definition.fire(DefinitionChange {
                current: self.current(),
                previous,
            });
        }
    }
}

fn downcast_value<T>(value: &DynamicValue) -> T
where
    T: Any + Clone + Send + Sync + 'static,
{
    Arc::downcast::<T>(value.clone())
        .expect("collection value type does not match its token")
        .as_ref()
        .clone()
}
