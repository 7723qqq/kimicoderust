use std::future::Future;
use std::sync::Arc;

use futures_util::{Stream, StreamExt, stream};
use tokio_util::sync::CancellationToken;

use crate::human::llm::requester::requester::LlmCredentialProvider;
use crate::wire::record::WireAsyncError;

pub async fn run_with_credential_recovery<T, F, Fut>(
    credential_provider: Option<&Arc<dyn LlmCredentialProvider>>,
    mut run: F,
    signal: Option<&CancellationToken>,
) -> Result<T, WireAsyncError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, WireAsyncError>>,
{
    match run().await {
        Ok(value) => Ok(value),
        Err(error) => {
            if signal.is_some_and(CancellationToken::is_cancelled)
                || !credential_provider.is_some_and(|provider| provider.can_recover(&error))
            {
                return Err(error);
            }
            if let Some(provider) = credential_provider {
                provider.invalidate();
            }
            run().await
        }
    }
}

struct StreamRecoveryState<F, S> {
    credential_provider: Option<Arc<dyn LlmCredentialProvider>>,
    stream_factory: F,
    current: Option<S>,
    signal: Option<CancellationToken>,
    recovered: bool,
}

pub fn stream_with_credential_recovery<T, F, S>(
    credential_provider: Option<Arc<dyn LlmCredentialProvider>>,
    stream_factory: F,
    signal: Option<CancellationToken>,
) -> impl Stream<Item = Result<T, WireAsyncError>>
where
    F: FnMut() -> S,
    S: Stream<Item = Result<T, WireAsyncError>> + Unpin,
{
    let initial = StreamRecoveryState {
        credential_provider,
        stream_factory,
        current: None,
        signal,
        recovered: false,
    };
    stream::try_unfold(initial, |mut state| async move {
        loop {
            if state.current.is_none() {
                state.current = Some((state.stream_factory)());
            }
            let current = state.current.as_mut().unwrap();
            match current.next().await {
                Some(Ok(item)) => return Ok(Some((item, state))),
                Some(Err(error)) => {
                    if state.recovered
                        || state
                            .signal
                            .as_ref()
                            .is_some_and(CancellationToken::is_cancelled)
                        || !state
                            .credential_provider
                            .as_ref()
                            .is_some_and(|provider| provider.can_recover(&error))
                    {
                        return Err(error);
                    }
                    state.recovered = true;
                    if let Some(provider) = &state.credential_provider {
                        provider.invalidate();
                    }
                    state.current = None;
                }
                None => return Ok(None),
            }
        }
    })
}
