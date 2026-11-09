
use std::fmt;
use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitTimeout {
    pub operation: String,
    pub stage: String,
    pub limit: Duration,
    pub waited: Duration,
}

impl fmt::Display for WaitTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} timed out while {}: waited {:.1}s (limit {:.1}s)",
            self.operation,
            self.stage,
            self.waited.as_secs_f64(),
            self.limit.as_secs_f64()
        )
    }
}

impl std::error::Error for WaitTimeout {}

#[derive(Debug, Clone)]
pub struct Deadline {
    operation: String,
    limit: Option<Duration>,
}

impl Deadline {
    #[must_use]
    pub fn new(operation: impl Into<String>, limit: Duration) -> Self {
        Self {
            operation: operation.into(),
            limit: Some(limit),
        }
    }

    #[must_use]
    pub fn unbounded(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            limit: None,
        }
    }

    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.limit.unwrap_or(Duration::MAX)
    }

    pub async fn wait<T>(
        &self,
        stage: impl Into<String>,
        future: impl Future<Output = T>,
    ) -> Result<T, WaitTimeout> {
        let stage = stage.into();
        let Some(limit) = self.limit else {
            return Ok(future.await);
        };
        match tokio::time::timeout(limit, future).await {
            Ok(value) => Ok(value),
            Err(_) => Err(WaitTimeout {
                operation: self.operation.clone(),
                stage,
                limit,
                waited: limit,
            }),
        }
    }
}

pub async fn bounded<T>(
    operation: impl Into<String>,
    stage: impl Into<String>,
    limit: Duration,
    future: impl Future<Output = T>,
) -> Result<T, WaitTimeout> {
    Deadline::new(operation, limit).wait(stage, future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_wait_inside_the_bound_returns_the_value() {
        let value = bounded(
            "test op",
            "waiting for a value",
            Duration::from_secs(30),
            async { 41 },
        )
        .await;
        assert_eq!(value, Ok(41));
    }

    #[tokio::test]
    async fn a_wait_past_the_bound_reports_what_where_and_how_long() {
        let error = bounded(
            "test op",
            "waiting for a channel that never closes",
            Duration::from_millis(20),
            std::future::pending::<()>(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.operation, "test op");
        assert_eq!(error.stage, "waiting for a channel that never closes");
        assert_eq!(error.limit, Duration::from_millis(20));
        assert_eq!(error.waited, Duration::from_millis(20));
        let rendered = error.to_string();
        assert!(rendered.contains("test op"), "{rendered}");
        assert!(rendered.contains("waiting for a channel"), "{rendered}");
        assert!(rendered.contains("limit"), "{rendered}");
    }

    #[tokio::test]
    async fn each_wait_gets_the_full_bound_not_what_the_previous_one_left() {
        let deadline = Deadline::new("two stages", Duration::from_millis(60));
        deadline
            .wait("first stage", tokio::time::sleep(Duration::from_millis(40)))
            .await
            .unwrap();
        // The reference arms a fresh timer per wait, so the second wait is not
        // charged for the first one's elapsed time.
        deadline
            .wait(
                "second stage",
                tokio::time::sleep(Duration::from_millis(40)),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_unbounded_wait_returns_when_the_future_does() {
        let value = Deadline::unbounded("test op")
            .wait("waiting for a value", async { 7 })
            .await;
        assert_eq!(value, Ok(7));
    }

    #[test]
    fn an_unbounded_wait_reports_no_limit() {
        let deadline = Deadline::unbounded("op");
        assert_eq!(deadline.remaining(), Duration::MAX);
    }
}
