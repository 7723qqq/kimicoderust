use std::sync::Arc;

use serde_json::Value;

use crate::rpc::types::BoxFuture;

#[derive(Debug, Clone)]
pub struct BeforeExecuteContext {
    pub agent_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
}

#[derive(Debug, Clone)]
pub enum BeforeExecuteDecision {
    Veto(String),

    Allow,

    Pass(Value),
}

#[derive(Debug, Clone, Default)]
pub struct BeforeExecuteOutcome {
    pub veto: Option<String>,

    pub failure: Option<String>,

    pub metadata: Option<Value>,
}

pub type BeforeExecuteListener = Arc<
    dyn Fn(
            BeforeExecuteContext,
        ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, String>>
        + Send
        + Sync,
>;

#[derive(Default)]
pub struct BeforeExecuteGate {
    listeners: Vec<BeforeExecuteListener>,
}

impl BeforeExecuteGate {
    pub fn new(listeners: Vec<BeforeExecuteListener>) -> Self {
        Self { listeners }
    }

    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }

    pub async fn run(&self, context: BeforeExecuteContext) -> BeforeExecuteOutcome {
        let mut outcome = BeforeExecuteOutcome::default();
        for listener in &self.listeners {
            match listener(context.clone()).await {
                Ok(Some(BeforeExecuteDecision::Veto(reason))) => {
                    outcome.veto = Some(reason);
                    outcome.metadata = None;
                    return outcome;
                }
                Ok(Some(BeforeExecuteDecision::Allow)) => return outcome,
                Ok(Some(BeforeExecuteDecision::Pass(metadata))) => {
                    if outcome.metadata.is_none() {
                        outcome.metadata = Some(metadata);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    outcome.failure = Some(error);
                    outcome.metadata = None;
                    return outcome;
                }
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn context() -> BeforeExecuteContext {
        BeforeExecuteContext {
            agent_id: "main".into(),
            turn_id: "1".into(),
            tool_call_id: "call-1".into(),
            tool_name: "Bash".into(),
            args: json!({ "command": "echo hi" }),
        }
    }

    fn listener(
        seen: Arc<Mutex<Vec<&'static str>>>,
        name: &'static str,
        decision: Option<BeforeExecuteDecision>,
    ) -> BeforeExecuteListener {
        Arc::new(move |_| {
            seen.lock().unwrap().push(name);
            let decision = decision.clone();
            Box::pin(async move { Ok(decision) })
        })
    }

    #[tokio::test]
    async fn the_first_veto_wins_and_stops_the_chain() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = BeforeExecuteGate::new(vec![
            listener(seen.clone(), "first", None),
            listener(
                seen.clone(),
                "veto",
                Some(BeforeExecuteDecision::Veto("no".into())),
            ),
            listener(seen.clone(), "later", None),
        ]);
        let outcome = gate.run(context()).await;
        assert_eq!(outcome.veto.as_deref(), Some("no"));
        assert_eq!(*seen.lock().unwrap(), vec!["first", "veto"]);
    }

    #[tokio::test]
    async fn a_listener_error_fails_the_call_instead_of_passing() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let erroring: BeforeExecuteListener =
            Arc::new(|_| Box::pin(async { Err("boom".to_string()) }));
        let gate = BeforeExecuteGate::new(vec![
            listener(seen.clone(), "first", None),
            erroring,
            listener(seen.clone(), "later", None),
        ]);
        let outcome = gate.run(context()).await;
        assert_eq!(outcome.failure.as_deref(), Some("boom"));
        assert_eq!(*seen.lock().unwrap(), vec!["first"]);
    }

    #[tokio::test]
    async fn allow_stops_the_remaining_listeners_without_vetoing() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = BeforeExecuteGate::new(vec![
            listener(seen.clone(), "allow", Some(BeforeExecuteDecision::Allow)),
            listener(
                seen.clone(),
                "veto",
                Some(BeforeExecuteDecision::Veto("no".into())),
            ),
        ]);
        let outcome = gate.run(context()).await;
        assert!(outcome.veto.is_none());
        assert_eq!(*seen.lock().unwrap(), vec!["allow"]);
    }

    #[tokio::test]
    async fn the_first_metadata_wins_and_pass_does_not_stop_the_chain() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = BeforeExecuteGate::new(vec![
            listener(
                seen.clone(),
                "one",
                Some(BeforeExecuteDecision::Pass(json!({ "source": "one" }))),
            ),
            listener(
                seen.clone(),
                "two",
                Some(BeforeExecuteDecision::Pass(json!({ "source": "two" }))),
            ),
            listener(
                seen.clone(),
                "three",
                Some(BeforeExecuteDecision::Pass(json!({ "source": "three" }))),
            ),
        ]);
        let outcome = gate.run(context()).await;
        assert!(outcome.veto.is_none());
        assert_eq!(outcome.metadata, Some(json!({ "source": "one" })));
        assert_eq!(*seen.lock().unwrap(), vec!["one", "two", "three"]);
    }

    #[tokio::test]
    async fn a_failing_listener_fails_the_call_and_later_listeners_do_not_run() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let failing: BeforeExecuteListener = Arc::new(|_| Box::pin(async { Err("boom".into()) }));
        let gate = BeforeExecuteGate::new(vec![
            failing,
            listener(
                seen.clone(),
                "veto",
                Some(BeforeExecuteDecision::Veto("no".into())),
            ),
        ]);
        let outcome = gate.run(context()).await;
        assert_eq!(outcome.failure.as_deref(), Some("boom"));
        assert_eq!(outcome.veto, None);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_deferred_listener_is_just_an_async_body() {
        let gate = BeforeExecuteGate::new(vec![Arc::new(|context: BeforeExecuteContext| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(Some(BeforeExecuteDecision::Veto(format!(
                    "deferred veto for {}",
                    context.tool_name
                ))))
            })
        })]);
        let outcome = gate.run(context()).await;
        assert_eq!(outcome.veto.as_deref(), Some("deferred veto for Bash"));
    }
}
