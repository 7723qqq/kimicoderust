use std::error::Error;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::_base::utils::abort::{AbortError, AbortSignal, UserCancellationError, abort_error};
use crate::wire::record::WireAsyncError;

const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorized</title></head><body style=\"font-family:system-ui,sans-serif;padding:2rem;\"><h1>Sign-in complete</h1><p>You can close this tab and return to the application.</p></body></html>";
const ERROR_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>OAuth error</title></head><body style=\"font-family:system-ui,sans-serif;padding:2rem;\"><h1>Sign-in failed</h1><p>The authorization server reported an error. Return to the application for details.</p></body></html>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallbackClosedError;

impl fmt::Display for OAuthCallbackClosedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OAuth callback listener closed")
    }
}

impl Error for OAuthCallbackClosedError {}

#[derive(Debug, Clone)]
enum CallbackFailureKind {
    Closed,
    Abort(AbortError),
    UserCancellation(UserCancellationError),
    Message,
}

#[derive(Debug, Clone)]
struct CallbackFailure {
    message: Arc<String>,
    kind: CallbackFailureKind,
}

impl CallbackFailure {
    fn new(error: WireAsyncError) -> Self {
        let kind = if error.downcast_ref::<OAuthCallbackClosedError>().is_some() {
            CallbackFailureKind::Closed
        } else if let Some(error) = error.downcast_ref::<AbortError>() {
            CallbackFailureKind::Abort(error.clone())
        } else if let Some(error) = error.downcast_ref::<UserCancellationError>() {
            CallbackFailureKind::UserCancellation(error.clone())
        } else {
            CallbackFailureKind::Message
        };
        Self {
            message: Arc::new(error.to_string()),
            kind,
        }
    }

    fn closed() -> Self {
        Self::new(Box::new(OAuthCallbackClosedError))
    }

    fn message(message: impl Into<String>) -> Self {
        Self {
            message: Arc::new(message.into()),
            kind: CallbackFailureKind::Message,
        }
    }

    fn abort(signal: Option<&AbortSignal>) -> Self {
        let Some(reason) = signal.and_then(AbortSignal::reason) else {
            return Self::new(Box::new(abort_error()));
        };
        let kind = if let Some(error) = reason.downcast_ref::<AbortError>() {
            CallbackFailureKind::Abort(error.clone())
        } else if let Some(error) = reason.downcast_ref::<UserCancellationError>() {
            CallbackFailureKind::UserCancellation(error.clone())
        } else {
            CallbackFailureKind::Message
        };
        Self {
            message: Arc::new(reason.to_string()),
            kind,
        }
    }

    fn boxed(&self) -> WireAsyncError {
        match &self.kind {
            CallbackFailureKind::Closed => Box::new(OAuthCallbackClosedError),
            CallbackFailureKind::Abort(error) => Box::new(error.clone()),
            CallbackFailureKind::UserCancellation(error) => Box::new(error.clone()),
            CallbackFailureKind::Message => Box::new(CallbackMessageError(self.message.clone())),
        }
    }
}

#[derive(Debug)]
struct CallbackMessageError(Arc<String>);

impl fmt::Display for CallbackMessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for CallbackMessageError {}

enum CallbackOutcome {
    Pending,
    Resolved(CallbackResult),
    Rejected(CallbackFailure),
}

struct CallbackWaiter {
    id: u64,
    sender: tokio::sync::oneshot::Sender<Result<CallbackResult, WireAsyncError>>,
}

struct CallbackState {
    outcome: CallbackOutcome,
    waiter: Option<CallbackWaiter>,
    settled_waiter: Option<u64>,
    next_waiter_id: u64,
    sockets: HashMap<usize, Arc<tokio::sync::Mutex<tokio::net::TcpStream>>>,
    next_socket_id: usize,
    accept_task: Option<tokio::task::JoinHandle<()>>,
    close_future: Option<futures_util::future::Shared<futures_util::future::BoxFuture<'static, ()>>>,
    close_token: tokio_util::sync::CancellationToken,
}

pub struct CallbackServer {
    pub redirect_uri: String,
    state: Arc<Mutex<CallbackState>>,
}

impl CallbackServer {
    pub fn wait_for_code(
        &self,
        signal: Option<AbortSignal>,
        timeout_ms: Option<u64>,
    ) -> futures_util::future::BoxFuture<'static, Result<CallbackResult, WireAsyncError>> {
        let state = self.state.clone();
        futures_util::FutureExt::boxed(async move {
            let (id, mut receiver) = {
                let mut state = state.lock();
                match &state.outcome {
                    CallbackOutcome::Resolved(result) => return Ok(result.clone()),
                    CallbackOutcome::Rejected(error) => return Err(error.boxed()),
                    CallbackOutcome::Pending => {}
                }
                let id = state.next_waiter_id;
                state.next_waiter_id += 1;
                let (sender, receiver) = tokio::sync::oneshot::channel();
                state.waiter = Some(CallbackWaiter { id, sender });
                (id, Some(receiver))
            };
            let abort_signal = signal.clone();
            let abort = async {
                if let Some(signal) = abort_signal {
                    if signal.aborted() {
                        return;
                    }
                    signal.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let timeout = async {
                if let Some(timeout_ms) = timeout_ms {
                    tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            futures_util::pin_mut!(abort);
            futures_util::pin_mut!(timeout);
            loop {
                let receive = async {
                    let Some(receiver) = receiver.take() else {
                        return std::future::pending::<Option<Result<CallbackResult, WireAsyncError>>>().await;
                    };
                    match receiver.await {
                        Ok(result) => Some(result),
                        Err(_) => {
                            let current = state
                                .lock()
                                .waiter
                                .as_ref()
                                .is_some_and(|waiter| waiter.id == id);
                            if current {
                                Some(Err(CallbackFailure::closed().boxed()))
                            } else {
                                None
                            }
                        }
                    }
                };
                tokio::select! {
                    result = receive => {
                        if let Some(result) = result {
                            return result;
                        }
                    }
                    _ = &mut abort => {
                        let failure = CallbackFailure::abort(signal.as_ref());
                        if let Some(result) = settle_waiter(&state, id, failure) {
                            return result;
                        }
                        return std::future::pending::<Result<CallbackResult, WireAsyncError>>().await;
                    }
                    _ = &mut timeout => {
                        let failure = CallbackFailure::message("OAuth callback timed out");
                        if let Some(result) = settle_waiter(&state, id, failure) {
                            return result;
                        }
                        return std::future::pending::<Result<CallbackResult, WireAsyncError>>().await;
                    }
                }
            }
        })
    }

    pub async fn close(&self) {
        {
            let mut state = self.state.lock();
            if matches!(state.outcome, CallbackOutcome::Pending) {
                let failure = CallbackFailure::closed();
                state.outcome = CallbackOutcome::Rejected(failure.clone());
                if let Some(waiter) = state.waiter.take() {
                    state.settled_waiter = Some(waiter.id);
                    let _ = waiter.sender.send(Err(failure.boxed()));
                }
            }
        }
        close_callback_server(&self.state).await;
    }
}

pub async fn start_callback_server() -> Result<CallbackServer, WireAsyncError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(Mutex::new(CallbackState {
        outcome: CallbackOutcome::Pending,
        waiter: None,
        settled_waiter: None,
        next_waiter_id: 1,
        sockets: HashMap::new(),
        next_socket_id: 0,
        accept_task: None,
        close_future: None,
        close_token: tokio_util::sync::CancellationToken::new(),
    }));
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let accept_state = state.clone();
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(_) => break,
            };
            let socket = Arc::new(tokio::sync::Mutex::new(socket));
            let id = {
                let mut state = accept_state.lock();
                let id = state.next_socket_id;
                state.next_socket_id += 1;
                state.sockets.insert(id, socket.clone());
                id
            };
            let state = accept_state.clone();
            let expected_host = format!("127.0.0.1:{port}");
            tokio::spawn(async move {
                let _ = handle_connection(socket.clone(), expected_host, state.clone(), id).await;
                state.lock().sockets.remove(&id);
            });
        }
    });
    state.lock().accept_task = Some(task);
    Ok(CallbackServer {
        redirect_uri,
        state,
    })
}

async fn handle_connection(
    socket: Arc<tokio::sync::Mutex<tokio::net::TcpStream>>,
    expected_host: String,
    callback_state: Arc<Mutex<CallbackState>>,
    socket_id: usize,
) -> Result<(), WireAsyncError> {
    let close_token = callback_state.lock().close_token.clone();
    let mut socket = socket.lock().await;
    let mut input = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        let read = tokio::select! {
            result = socket.read(&mut buffer) => result?,
            _ = close_token.cancelled() => return Ok(()),
        };
        if read == 0 {
            return Ok(());
        }
        input.extend_from_slice(&buffer[..read]);
        if input.len() > 64 * 1024 {
            return Ok(());
        }
        if input.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    callback_state.lock().sockets.remove(&socket_id);
    let text = String::from_utf8_lossy(&input);
    let Some(request_line) = text.lines().next() else {
        return Ok(());
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        write_response(&mut socket, 404, "").await?;
        return Ok(());
    }
    let host = text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host").then_some(value.trim())
        })
        .unwrap_or("");
    if host != expected_host {
        write_response(&mut socket, 404, "").await?;
        return Ok(());
    }
    let base = url::Url::parse("http://localhost")?;
    let url = match base.join(target) {
        Ok(url) => url,
        Err(_) => {
            write_response(&mut socket, 404, "").await?;
            return Ok(());
        }
    };
    if url.path() != "/callback" {
        write_response(&mut socket, 404, "").await?;
        return Ok(());
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string())
    };
    if let Some(error) = param("error") {
        let description = param("error_description").unwrap_or_default();
        let response = write_response(&mut socket, 400, ERROR_HTML).await;
        let _ = settle(
            &callback_state,
            CallbackFailure::message(format!(
                "OAuth error: {error}{}",
                if description.is_empty() { String::new() } else { format!(" — {description}") }
            )),
        );
        response?;
        return Ok(());
    }
    let code = param("code");
    if code.as_deref().unwrap_or("").is_empty() {
        let response = write_response(&mut socket, 400, ERROR_HTML).await;
        let _ = settle(
            &callback_state,
            CallbackFailure::message("OAuth callback missing authorization code"),
        );
        response?;
        return Ok(());
    }
    let state = param("state");
    let response = write_response(&mut socket, 200, SUCCESS_HTML).await;
    settle_resolved(
        &callback_state,
        CallbackResult {
            code: code.unwrap(),
            state,
        },
    );
    let _ = socket.shutdown().await;
    response?;
    Ok(())
}

fn settle_resolved(state: &Arc<Mutex<CallbackState>>, result: CallbackResult) {
    let waiter = {
        let mut state = state.lock();
        if !matches!(state.outcome, CallbackOutcome::Pending) {
            return;
        }
        state.outcome = CallbackOutcome::Resolved(result.clone());
        let waiter = state.waiter.take();
        if let Some(waiter) = &waiter {
            state.settled_waiter = Some(waiter.id);
        }
        waiter
    };
    if let Some(waiter) = waiter {
        let _ = waiter.sender.send(Ok(result));
    }
    let state = state.clone();
    tokio::spawn(async move {
        close_callback_server(&state).await;
    });
}

fn settled_wait_result(state: &Arc<Mutex<CallbackState>>, id: u64) -> Option<Result<CallbackResult, WireAsyncError>> {
    let state = state.lock();
    if state.settled_waiter != Some(id) {
        return None;
    }
    match &state.outcome {
        CallbackOutcome::Pending => None,
        CallbackOutcome::Resolved(result) => Some(Ok(result.clone())),
        CallbackOutcome::Rejected(failure) => Some(Err(failure.boxed())),
    }
}

fn settle_waiter(
    state: &Arc<Mutex<CallbackState>>,
    id: u64,
    failure: CallbackFailure,
) -> Option<Result<CallbackResult, WireAsyncError>> {
    if settle(state, failure.clone()) {
        let state = state.lock();
        if state.settled_waiter == Some(id) {
            return Some(Err(failure.boxed()));
        }
        return None;
    }
    settled_wait_result(state, id)
}

fn settle(state: &Arc<Mutex<CallbackState>>, failure: CallbackFailure) -> bool {
    let waiter = {
        let mut state = state.lock();
        if !matches!(state.outcome, CallbackOutcome::Pending) {
            return false;
        }
        state.outcome = CallbackOutcome::Rejected(failure.clone());
        let waiter = state.waiter.take();
        if let Some(waiter) = &waiter {
            state.settled_waiter = Some(waiter.id);
        }
        waiter
    };
    if let Some(waiter) = waiter {
        let _ = waiter.sender.send(Err(failure.boxed()));
    }
    let state = state.clone();
    tokio::spawn(async move {
        close_callback_server(&state).await;
    });
    true
}

fn close_callback_server(
    state: &Arc<Mutex<CallbackState>>,
) -> futures_util::future::Shared<futures_util::future::BoxFuture<'static, ()>> {
    let mut state_guard = state.lock();
    if let Some(future) = &state_guard.close_future {
        return future.clone();
    }
    let task = state_guard.accept_task.take();
    state_guard.close_token.cancel();
    let sockets = state_guard.sockets.clone();
    let future = async move {
        if let Some(task) = task {
            task.abort();
        }
        for (_, socket) in sockets {
            let _ = socket.lock().await.shutdown().await;
        }
    }
    .boxed()
    .shared();
    state_guard.close_future = Some(future.clone());
    drop(state_guard);
    tokio::spawn(future.clone());
    future
}

async fn write_response(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> Result<(), WireAsyncError> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let response = if status == 404 {
        format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\n\r\n")
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    socket.write_all(response.as_bytes()).await?;
    Ok(())
}
