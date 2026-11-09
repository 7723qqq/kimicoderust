//! Ordered hook slots, ported from v2 `hooks.ts` (reference HEAD `a0ccd487`).
//!
//! v2 lets a service wrap another service's async operation with an ordered
//! chain: every handler receives the context plus a `next` continuation it may
//! call (optionally with a replacement context) to run the rest of the chain.
//! Registration is id-keyed, and `before` / `after` place a handler relative
//! to an already registered one instead of appending it.
//!
//! The Rust port keeps the same contract. Differences are mechanical: a
//! registration is a RAII [`HookRegistration`] instead of an `IDisposable`
//! (dropping it removes the entry), and a handler returns a boxed future
//! instead of being an `async` function, because a Rust closure cannot be
//! `async` and recursive in the same signature.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// The continuation every hook handler receives.
pub type BoxHookFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// One hook handler: the context plus the continuation that runs the rest of
/// the chain (v2 `HookHandler<TContext>`).
pub type HookHandler<T> = Arc<dyn Fn(T, HookNext<T>) -> BoxHookFuture + Send + Sync + 'static>;

/// Placement of a new handler relative to an existing one (v2
/// `HookRegisterOptions`). At most one of the two may be set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookRegisterOptions {
    pub before: Option<String>,
    pub after: Option<String>,
}

impl HookRegisterOptions {
    #[must_use]
    pub fn before(id: impl Into<String>) -> Self {
        Self {
            before: Some(id.into()),
            after: None,
        }
    }

    #[must_use]
    pub fn after(id: impl Into<String>) -> Self {
        Self {
            before: None,
            after: Some(id.into()),
        }
    }
}

struct HookEntry<T> {
    id: String,
    handler: HookHandler<T>,
}

type EntryList<T> = Arc<Mutex<Vec<Arc<HookEntry<T>>>>>;

/// A chain of handlers sharing one context type (v2 `OrderedHookSlot<T>`).
pub struct OrderedHookSlot<T> {
    entries: EntryList<T>,
}

impl<T> Clone for OrderedHookSlot<T> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<T> Default for OrderedHookSlot<T> {
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<T: Clone + Send + 'static> OrderedHookSlot<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register `handler` under `id`, replacing any handler already using that
    /// id. `before` / `after` must name a registered id.
    ///
    /// # Errors
    ///
    /// Returns an error when both `before` and `after` are set, or when the
    /// named target is not registered — the same two cases where v2 throws a
    /// `BugIndicatingError`.
    pub fn register(
        &self,
        id: &str,
        handler: HookHandler<T>,
        options: HookRegisterOptions,
    ) -> Result<HookRegistration<T>, String> {
        if options.before.is_some() && options.after.is_some() {
            return Err("hook registration cannot specify both before and after".into());
        }
        let mut entries = self.lock();
        entries.retain(|entry| entry.id != id);
        let entry = Arc::new(HookEntry {
            id: id.to_string(),
            handler,
        });
        match options.before.as_ref().or(options.after.as_ref()) {
            None => entries.push(entry.clone()),
            Some(target) => {
                let index = entries
                    .iter()
                    .position(|candidate| &candidate.id == target)
                    .ok_or_else(|| format!("hook target \"{target}\" is not registered"))?;
                let insert_at = if options.before.is_some() {
                    index
                } else {
                    index + 1
                };
                entries.insert(insert_at, entry.clone());
            }
        }
        Ok(HookRegistration {
            entries: self.entries.clone(),
            entry,
        })
    }

    /// Remove the handler registered under `id`. Returns whether one existed.
    pub fn delete(&self, id: &str) -> bool {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        entries.len() != before
    }

    /// Whether `id` is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.lock().iter().any(|entry| entry.id == id)
    }

    /// Run the chain over `context`, with a no-op terminal.
    #[must_use]
    pub fn run(&self, context: T) -> BoxHookFuture {
        self.run_with_terminal(context, |_| Box::pin(async {}))
    }

    /// Run the chain over `context`; the terminal runs once the last handler
    /// calls `next` (v2 `run(context, terminal)`).
    #[must_use]
    pub fn run_with_terminal<F>(&self, context: T, terminal: F) -> BoxHookFuture
    where
        F: Fn(T) -> BoxHookFuture + Send + Sync + 'static,
    {
        // v2 snapshots the entry list before dispatch, so a handler that
        // registers or deletes another hook cannot change this run.
        let snapshot: Arc<Vec<Arc<HookEntry<T>>>> = Arc::new(self.lock().clone());
        HookNext {
            entries: snapshot,
            index: 0,
            context,
            terminal: Arc::new(terminal),
        }
        .proceed(None)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<HookEntry<T>>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// The continuation handed to a handler. Consuming it runs the handler at
/// `index` (or the terminal when the chain is exhausted).
pub struct HookNext<T> {
    entries: Arc<Vec<Arc<HookEntry<T>>>>,
    index: usize,
    context: T,
    terminal: Arc<dyn Fn(T) -> BoxHookFuture + Send + Sync>,
}

impl<T: Clone + Send + 'static> HookNext<T> {
    /// Continue the chain, optionally replacing the context for the remaining
    /// handlers (v2 `next(override?)`).
    #[must_use]
    pub fn proceed(self, override_context: Option<T>) -> BoxHookFuture {
        let context = override_context.unwrap_or(self.context);
        Box::pin(async move {
            match self.entries.get(self.index).cloned() {
                Some(entry) => {
                    let next = HookNext {
                        entries: self.entries.clone(),
                        index: self.index + 1,
                        context: context.clone(),
                        terminal: self.terminal.clone(),
                    };
                    (entry.handler)(context, next).await;
                }
                None => (self.terminal)(context).await,
            }
        })
    }
}

/// Detaches a handler from its slot when [`HookRegistration::dispose`] is
/// called (v2 `IDisposable`).
///
/// Unlike the JS original this is not tied to `Drop`: a registration the
/// caller ignores must stay active, which is the common case for a service
/// that installs its handlers for the lifetime of the process. Call `dispose`
/// explicitly to remove it.
///
/// A handler that was already replaced by a later registration with the same
/// id is no longer in the list, so disposing this handle is a no-op rather
/// than removing the replacement.
pub struct HookRegistration<T> {
    entries: EntryList<T>,
    entry: Arc<HookEntry<T>>,
}

impl<T> HookRegistration<T> {
    /// Remove this exact registration, leaving a same-id replacement in place.
    pub fn dispose(self) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        entries.retain(|candidate| !Arc::ptr_eq(candidate, &self.entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[derive(Clone)]
    struct Ctx {
        seen: Arc<StdMutex<Vec<(&'static str, i32)>>>,
        value: i32,
    }

    fn block_on(future: impl Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future);
    }

    fn recording_handler(id: &'static str) -> HookHandler<Ctx> {
        Arc::new(move |context: Ctx, next: HookNext<Ctx>| {
            context.seen.lock().unwrap().push((id, context.value));
            next.proceed(None)
        })
    }

    fn ctx() -> Ctx {
        Ctx {
            seen: Arc::new(StdMutex::new(Vec::new())),
            value: 1,
        }
    }

    #[test]
    fn handlers_run_in_registration_order() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register("a", recording_handler("a"), HookRegisterOptions::default())
            .unwrap();
        slot.register("b", recording_handler("b"), HookRegisterOptions::default())
            .unwrap();
        slot.register("c", recording_handler("c"), HookRegisterOptions::default())
            .unwrap();
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert_eq!(*seen.lock().unwrap(), vec![("a", 1), ("b", 1), ("c", 1)]);
    }

    #[test]
    fn before_and_after_place_the_handler_relative_to_the_target() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register(
            "middle",
            recording_handler("middle"),
            HookRegisterOptions::default(),
        )
        .unwrap();
        slot.register(
            "first",
            recording_handler("first"),
            HookRegisterOptions::before("middle"),
        )
        .unwrap();
        slot.register(
            "last",
            recording_handler("last"),
            HookRegisterOptions::after("middle"),
        )
        .unwrap();
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("first", 1), ("middle", 1), ("last", 1)]
        );
    }

    #[test]
    fn registering_the_same_id_replaces_the_previous_handler() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register("a", recording_handler("a"), HookRegisterOptions::default())
            .unwrap();
        slot.register("b", recording_handler("b"), HookRegisterOptions::default())
            .unwrap();
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert_eq!(*seen.lock().unwrap(), vec![("a", 1), ("b", 1)]);
        assert!(slot.contains("a"));
    }

    #[test]
    fn before_and_after_together_is_rejected() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        let options = HookRegisterOptions {
            before: Some("x".into()),
            after: Some("y".into()),
        };
        assert!(slot.register("a", recording_handler("a"), options).is_err());
    }

    #[test]
    fn an_unknown_placement_target_is_rejected() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        assert!(
            slot.register(
                "a",
                recording_handler("a"),
                HookRegisterOptions::after("missing")
            )
            .is_err()
        );
    }

    #[test]
    fn delete_removes_the_handler() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register("a", recording_handler("a"), HookRegisterOptions::default())
            .unwrap();
        assert!(slot.delete("a"));
        assert!(!slot.delete("a"));
        assert!(!slot.contains("a"));
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn disposing_the_registration_removes_the_handler() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        let registration = slot
            .register("a", recording_handler("a"), HookRegisterOptions::default())
            .unwrap();
        registration.dispose();
        assert!(!slot.contains("a"));
    }

    #[test]
    fn a_replaced_handler_is_not_removed_by_the_stale_registration() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        let stale = slot
            .register("a", recording_handler("a"), HookRegisterOptions::default())
            .unwrap();
        slot.register("a", recording_handler("a2"), HookRegisterOptions::default())
            .unwrap();
        stale.dispose();
        assert!(slot.contains("a"));
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert_eq!(*seen.lock().unwrap(), vec![("a2", 1)]);
    }

    #[test]
    fn proceed_can_replace_the_context_for_the_rest_of_the_chain() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register(
            "rewrite",
            Arc::new(|mut context: Ctx, next: HookNext<Ctx>| {
                context.value = 7;
                next.proceed(Some(context))
            }),
            HookRegisterOptions::default(),
        )
        .unwrap();
        slot.register(
            "observe",
            recording_handler("observe"),
            HookRegisterOptions::default(),
        )
        .unwrap();
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert_eq!(*seen.lock().unwrap(), vec![("observe", 7)]);
    }

    #[test]
    fn a_handler_that_never_calls_next_stops_the_chain() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register(
            "stop",
            Arc::new(|_context: Ctx, _next: HookNext<Ctx>| Box::pin(async {})),
            HookRegisterOptions::default(),
        )
        .unwrap();
        slot.register(
            "unreached",
            recording_handler("unreached"),
            HookRegisterOptions::default(),
        )
        .unwrap();
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_terminal_runs_after_the_last_handler() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        slot.register("a", recording_handler("a"), HookRegisterOptions::default())
            .unwrap();
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let sink = observed.clone();
        block_on(slot.run_with_terminal(ctx(), move |context: Ctx| {
            sink.lock().unwrap().push(context.value);
            Box::pin(async {})
        }));
        assert_eq!(observed.lock().unwrap().as_slice(), &[1]);
    }

    #[test]
    fn a_handler_registered_during_a_run_does_not_join_that_run() {
        let slot: OrderedHookSlot<Ctx> = OrderedHookSlot::new();
        let late = slot.clone();
        slot.register(
            "adds",
            Arc::new(move |_context: Ctx, next: HookNext<Ctx>| {
                late.register(
                    "late",
                    recording_handler("late"),
                    HookRegisterOptions::default(),
                )
                .unwrap();
                next.proceed(None)
            }),
            HookRegisterOptions::default(),
        )
        .unwrap();
        let context = ctx();
        let seen = context.seen.clone();
        block_on(slot.run(context));
        assert!(seen.lock().unwrap().is_empty());
        assert!(slot.contains("late"));
    }
}
