use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use indexmap::IndexMap;
use parking_lot::Mutex;

use super::cascade_engine::CascadeScopeHandle;
use super::collection::{CollectionOptions, StoredRecord, collection};
use super::fiber::{FiberRuntime, ServiceRecipe, scope_units};
use super::instantiation_service::InstantiationService;
use super::scope::ScopeKind;
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::lifecycle::ledger::{Ledger, LedgerEntry};

type Retract = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
type Materialize = Arc<dyn Fn(StoredRecord) + Send + Sync>;

pub fn watch_scope_units(container: Arc<InstantiationService>, kind: ScopeKind) {
    if container.cascade_disposed() {
        return;
    }
    let token = scope_units(&kind);
    let host = container.fiber_host();
    let view = host.collection_view(collection::<super::descriptors::DynamicValue>(
        token.name.clone(),
        CollectionOptions::default(),
    ));
    let fold_ledger = Ledger::new(&format!("scope-units:{kind}"));
    let anchor_ledger = fold_ledger.clone();
    container.anchor_kernel_entry(
        Arc::new(move |reason| anchor_ledger.teardown(reason)),
        &format!("scope-units:{kind}"),
    );
    let materialized = Arc::new(Mutex::new(IndexMap::<u64, Retract>::new()));
    let materialize: Materialize = Arc::new({
        let host = host.clone();
        let fold_ledger = fold_ledger.clone();
        let materialized = materialized.clone();
        let kind = kind.clone();
        move |record| {
            let recipe = Arc::downcast::<ServiceRecipe>(record.value.clone())
                .expect("scope unit record is not a service recipe")
                .as_ref()
                .clone();
            let name = record.provider_name.clone();
            let unit_ledger = Ledger::new(&format!("scope-units:{kind}:{name}"));
            let result = (|| -> Result<(), String> {
                match recipe.clone() {
                    ServiceRecipe::Class(recipe) => {
                        let instance = host.construct_service(recipe, None)?;
                        unit_ledger.register(
                            move |_| {
                                if let Some(disposable) = instance.disposable {
                                    disposable.dispose();
                                }
                            },
                            &format!("unit:{name}"),
                        );
                    }
                    _ => {
                        let facade = Arc::new(FiberRuntime::new(
                            host.clone(),
                            unit_ledger.clone(),
                            name.clone(),
                            None,
                            None,
                            recipe.statics().inject.iter().cloned().collect(),
                            None,
                        ));
                        let out = match &recipe {
                            ServiceRecipe::Function(recipe) => {
                                (recipe.apply)(facade, None)?
                            }
                            ServiceRecipe::Object(recipe) => recipe.apply(facade, None)?,
                            ServiceRecipe::Class(_) => unreachable!(),
                        };
                        let out = Arc::new(Mutex::new(Some(out)));
                        unit_ledger.effect(
                            Arc::new(move || out.lock().take().unwrap_or(crate::_base::lifecycle::disposer::EffectResult::None)),
                            &format!("effect:{name}"),
                        );
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                if let Some(future) = unit_ledger.teardown("unload") {
                    tokio::spawn(future);
                }
                on_unexpected_error(error);
                return;
            }
            let retracted = Arc::new(AtomicBool::new(false));
            let provider_entry = Arc::new(Mutex::new(None::<LedgerEntry>));
            let retract: Retract = Arc::new({
                let retracted = retracted.clone();
                let provider_entry = provider_entry.clone();
                let materialized = materialized.clone();
                let unit_ledger = unit_ledger.clone();
                let id = record.id;
                move || {
                    if retracted.swap(true, Ordering::SeqCst) {
                        return async {}.boxed();
                    }
                    if let Some(entry) = provider_entry.lock().take() {
                        entry.release();
                    }
                    materialized.lock().remove(&id);
                    let teardown = unit_ledger.teardown("unload");
                    async move {
                        if let Some(teardown) = teardown {
                            teardown.await;
                        }
                    }
                    .boxed()
                }
            });
            if !record.provider_book.is_active() {
                let retract = retract.clone();
                tokio::spawn(async move { retract().await });
                return;
            }
            let provider_retract = retract.clone();
            *provider_entry.lock() = Some(record.provider_book.register(
                move |_| {
                    let future = provider_retract();
                    tokio::spawn(future);
                },
                &format!("scope-units:{kind}"),
            ));
            let fold_retract = retract.clone();
            fold_ledger.register(
                move |_| fold_retract(),
                &format!("record:{name}"),
            );
            materialized.lock().insert(record.id, retract);
        }
    });
    let reconcile = Arc::new({
        let container = container.clone();
        let fold_ledger = fold_ledger.clone();
        let materialized = materialized.clone();
        let materialize = materialize.clone();
        let kind = kind.clone();
        move || {
            if !fold_ledger.is_active() {
                return;
            }
            let erased = collection::<super::descriptors::DynamicValue>(
                format!("scope-units:{kind}"),
                CollectionOptions::default(),
            );
            let records = container
                .collection_store()
                .stored_records_for(&erased, container.scope_id());
            let mut seen = HashSet::new();
            for record in records {
                seen.insert(record.id);
                if !materialized.lock().contains_key(&record.id) {
                    materialize(record);
                }
            }
            let existing = materialized
                .lock()
                .iter()
                .map(|(id, retract)| (*id, retract.clone()))
                .collect::<Vec<_>>();
            for (id, retract) in existing {
                if !seen.contains(&id) {
                    let future = retract();
                    tokio::spawn(future);
                }
            }
        }
    });
    let subscription = view.on_did_change_event.subscribe({
        let reconcile = reconcile.clone();
        move |_| reconcile()
    });
    fold_ledger.register(
        move |_| subscription.dispose(),
        "view-subscription",
    );
    reconcile();
}
