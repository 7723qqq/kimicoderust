use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use crate::remote::process_lifecycle::{EXECUTOR_SHUTDOWN_GRACE_MS, LAUNCHER_TERMINATION_GRACE_MS};

pub trait BytePipe: Send + Sync {
    fn write(&self, chunk: &[u8]);
    fn end(&self);
    fn on_data(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>);
    fn on_end(&self, listener: Arc<dyn Fn() + Send + Sync>);
    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>);
}

pub struct ExecBridgeOptions {
    pub program: String,
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecBridgeError {
    pub message: Arc<String>,
}

impl std::fmt::Display for ExecBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecBridgeError {}

#[derive(Clone)]
pub struct ExecBridgeExit {
    pub code: Option<i32>,
    pub signal: Option<String>,
    pub error: Option<ExecBridgeError>,
}

struct BridgeState {
    data_listeners: Vec<Arc<dyn Fn(Vec<u8>) + Send + Sync>>,
    end_listeners: Vec<Arc<dyn Fn() + Send + Sync>>,
    error_listeners: Vec<Arc<dyn Fn(std::io::Error) + Send + Sync>>,
    stderr_tail: String,
    end_fired: bool,
    stdin_broken: bool,
    close_started: bool,
}

pub struct ExecBridge {
    state: Arc<Mutex<BridgeState>>,
    writer: tokio::sync::mpsc::UnboundedSender<WriterMessage>,
    exited: Shared<BoxFuture<'static, ExecBridgeExit>>,
    child: Arc<tokio::sync::Mutex<Child>>,
}

enum WriterMessage {
    Data(Vec<u8>),
    End,
}

impl ExecBridge {
    pub fn spawn(options: ExecBridgeOptions) -> std::io::Result<Arc<Self>> {
        let mut command = Command::new(&options.program);
        command
            .args(&options.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(false);
        if let Some(env) = options.env {
            command.env_clear();
            command.envs(env);
        }
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let child = command.spawn()?;
        Ok(Self::adopt(child))
    }

    pub fn adopt(mut child: Child) -> Arc<Self> {
        let state = Arc::new(Mutex::new(BridgeState {
            data_listeners: Vec::new(),
            end_listeners: Vec::new(),
            error_listeners: Vec::new(),
            stderr_tail: String::new(),
            end_fired: false,
            stdin_broken: false,
            close_started: false,
        }));
        let (writer, mut receiver) = tokio::sync::mpsc::unbounded_channel::<WriterMessage>();
        let mut stdin = child.stdin.take().unwrap();
        let writer_state = state.clone();
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                match message {
                    WriterMessage::Data(data) => {
                        if let Err(_) = stdin.write_all(&data).await {
                            break;
                        }
                    }
                    WriterMessage::End => {
                        let _ = stdin.shutdown().await;
                        return;
                    }
                }
            }
            writer_state.lock().stdin_broken = true;
            fire_end(&writer_state);
        });
        if let Some(mut stdout) = child.stdout.take() {
            let state = state.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0u8; 8192];
                loop {
                    match stdout.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(read) => {
                            let listeners = state.lock().data_listeners.clone();
                            let chunk = buffer[..read].to_vec();
                            for listener in listeners {
                                listener(chunk.clone());
                            }
                        }
                        Err(_) => break,
                    }
                }
                fire_end(&state);
            });
        }
        if let Some(mut stderr) = child.stderr.take() {
            let state = state.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = [0u8; 8192];
                while let Ok(read) = stderr.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let chunk = String::from_utf8_lossy(&buffer[..read]);
                    let mut state = state.lock();
                    state.stderr_tail.push_str(&chunk);
                    trim_stderr_tail(&mut state.stderr_tail);
                }
            });
        }
        let child = Arc::new(tokio::sync::Mutex::new(child));
        let wait_child = child.clone();
        let wait_state = state.clone();
        let exited = async move {
            let result = wait_child.lock().await.wait().await;
            match result {
                Ok(status) => {
                    wait_state.lock().stdin_broken = true;
                    fire_end(&wait_state);
                    ExecBridgeExit {
                        code: status.code(),
                        signal: exit_signal(&status),
                        error: None,
                    }
                }
                Err(error) => {
                    let listeners = wait_state.lock().error_listeners.clone();
                    for listener in listeners {
                        listener(std::io::Error::other(error.to_string()));
                    }
                    fire_end(&wait_state);
                    ExecBridgeExit {
                        code: None,
                        signal: None,
                        error: Some(ExecBridgeError {
                            message: Arc::new(error.to_string()),
                        }),
                    }
                }
            }
        }
        .boxed()
        .shared();
        tokio::spawn(exited.clone());
        Arc::new(Self {
            state,
            writer,
            exited,
            child,
        })
    }

    pub fn exited(&self) -> Shared<BoxFuture<'static, ExecBridgeExit>> {
        self.exited.clone()
    }

    pub fn get_stderr_tail(&self) -> String {
        self.state.lock().stderr_tail.clone()
    }

    pub fn close(self: &Arc<Self>) {
        self.close_with_timers();
    }

    pub fn close_with_timers(self: &Arc<Self>) {
        if !self.begin_close() {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(EXECUTOR_SHUTDOWN_GRACE_MS)) => {}
                _ = this.exited() => return,
            }
            {
                let mut child = this.child.lock().await;
                signal_child(&mut child, "SIGTERM");
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(LAUNCHER_TERMINATION_GRACE_MS)) => {}
                _ = this.exited() => return,
            }
            let mut child = this.child.lock().await;
            let _ = child.kill().await;
        });
    }

    fn begin_close(&self) -> bool {
        let mut state = self.state.lock();
        if state.close_started {
            return false;
        }
        state.close_started = true;
        let _ = self.writer.send(WriterMessage::End);
        true
    }
}

impl BytePipe for ExecBridge {
    fn write(&self, chunk: &[u8]) {
        if self.state.lock().stdin_broken {
            return;
        }
        let _ = self.writer.send(WriterMessage::Data(chunk.to_vec()));
    }

    fn end(&self) {
        if self.state.lock().stdin_broken {
            return;
        }
        let _ = self.writer.send(WriterMessage::End);
    }

    fn on_data(&self, listener: Arc<dyn Fn(Vec<u8>) + Send + Sync>) {
        self.state.lock().data_listeners.push(listener);
    }

    fn on_end(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        let mut state = self.state.lock();
        if state.end_fired {
            drop(state);
            listener();
        } else {
            state.end_listeners.push(listener);
        }
    }

    fn on_error(&self, listener: Arc<dyn Fn(std::io::Error) + Send + Sync>) {
        self.state.lock().error_listeners.push(listener);
    }
}

fn fire_end(state: &Arc<Mutex<BridgeState>>) {
    let listeners = {
        let mut state = state.lock();
        if state.end_fired {
            return;
        }
        state.end_fired = true;
        std::mem::take(&mut state.end_listeners)
    };
    for listener in listeners {
        listener();
    }
}

fn trim_stderr_tail(value: &mut String) {
    const LIMIT: usize = 64 * 1024;
    let units = value.encode_utf16().collect::<Vec<_>>();
    if units.len() > LIMIT {
        *value = String::from_utf16_lossy(&units[units.len() - LIMIT..]);
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    status.signal().map(|signal| {
        match signal {
            libc::SIGHUP => "SIGHUP".to_string(),
            libc::SIGINT => "SIGINT".to_string(),
            libc::SIGKILL => "SIGKILL".to_string(),
            libc::SIGTERM => "SIGTERM".to_string(),
            other => other.to_string(),
        }
    })
}

#[cfg(windows)]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn signal_child(child: &mut Child, signal: &str) {
    if let Some(pid) = child.id() {
        let signal = if signal == "SIGTERM" { libc::SIGTERM } else { libc::SIGKILL };
        unsafe {
            libc::kill(pid as i32, signal);
        }
    }
}

#[cfg(windows)]
fn signal_child(child: &mut Child, _signal: &str) {
    let _ = child.start_kill();
}
