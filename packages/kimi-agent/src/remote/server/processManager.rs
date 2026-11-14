use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::os::backends::host::host_process_service::HostProcessService;
use crate::os::interface::host_process::{IHostProcess, OS_PROCESS_SPAWN_FAILED};
use crate::remote::process_lifecycle::PROCESS_TERMINATION_GRACE_MS;
use crate::remote::protocol::errors::{
    RPC_INTERNAL_ERROR, RPC_INVALID_PARAMS, RPC_INVALID_REQUEST, RpcError,
};
use crate::remote::protocol::methods::*;
use crate::wire::record::WireAsyncError;

use super::fs_handler::{optional_boolean, require_absolute_path, require_params, require_string};

const TERMINATED_ID_CACHE_SIZE: usize = 4096;
const GROUP_REAP_INTERVAL_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    Starting,
    Running,
}

struct ManagedProcess {
    process_id: String,
    state: ProcessState,
    pid: i32,
    child: Option<Arc<dyn IHostProcess>>,
    stdin_open: bool,
    terminate_after_start: bool,
    client_paused: bool,
    exit_code: Option<i32>,
    closed: bool,
    open_streams: usize,
    stdin_waiters: Vec<Arc<tokio::sync::Notify>>,
    kill_timer: Option<tokio::task::JoinHandle<()>>,
}

struct ExitedProcessGroup {
    serial: u64,
    pid: i32,
    kill_timer: Option<tokio::task::JoinHandle<()>>,
    expiry_timer: Option<tokio::task::JoinHandle<()>>,
}

struct ManagerState {
    processes: IndexMap<String, Arc<Mutex<ManagedProcess>>>,
    terminated_ids: IndexSet<String>,
    exited_groups: IndexMap<String, ExitedProcessGroup>,
    next_exited_group_serial: u64,
    output_paused: bool,
    shutting_down: bool,
    disposed: bool,
}

pub struct ProcessManager {
    notify: Arc<dyn Fn(&str, Value) + Send + Sync>,
    host: HostProcessService,
    state: Arc<Mutex<ManagerState>>,
}

impl ProcessManager {
    pub fn new(notify: Arc<dyn Fn(&str, Value) + Send + Sync>, host: HostProcessService) -> Self {
        Self {
            notify,
            host,
            state: Arc::new(Mutex::new(ManagerState {
                processes: IndexMap::new(),
                terminated_ids: IndexSet::new(),
                exited_groups: IndexMap::new(),
                next_exited_group_serial: 0,
                output_paused: false,
                shutting_down: false,
                disposed: false,
            })),
        }
    }

    pub async fn start(&self, raw_params: Value) -> Result<ProcessStartResult, WireAsyncError> {
        {
            let state = self.state.lock();
            if state.disposed || state.shutting_down {
                return Err(RpcError::new(RPC_INTERNAL_ERROR, "server is shutting down", None).into());
            }
        }
        let params = require_params(raw_params)?;
        let process_id = require_string(&params, "processId")?;
        if process_id.encode_utf16().count() > 128 {
            return Err(RpcError::new(RPC_INVALID_PARAMS, "processId must be at most 128 characters", None).into());
        }
        let argv = params.get("argv").and_then(Value::as_array).ok_or_else(|| {
            RpcError::new(
                RPC_INVALID_PARAMS,
                "argv must be a string array with a non-empty first element",
                None,
            )
        })?;
        if argv.is_empty() || argv.iter().any(|item| !item.is_string()) || argv[0].as_str().unwrap_or("").is_empty() {
            return Err(RpcError::new(RPC_INVALID_PARAMS, "argv must be a string array with a non-empty first element", None).into());
        }
        let argv = argv.iter().map(|item| item.as_str().unwrap().to_string()).collect::<Vec<_>>();
        let cwd = require_absolute_path(&params, "cwd")?;
        if !tokio::fs::metadata(&cwd).await.ok().is_some_and(|stat| stat.is_dir()) {
            let mut data = Map::new();
            data.insert("domainCode".to_string(), OS_PROCESS_SPAWN_FAILED.into());
            data.insert("cwd".to_string(), cwd.clone().into());
            return Err(RpcError::new(
                RPC_INVALID_PARAMS,
                format!("cwd {cwd} does not exist or is not a directory"),
                Some(Value::Object(data)),
            ).into());
        }
        let env = match params.get("env") {
            None => None,
            Some(Value::Object(env)) => {
                let mut parsed = HashMap::new();
                for (key, value) in env {
                    let Some(value) = value.as_str() else {
                        return Err(RpcError::new(RPC_INVALID_PARAMS, format!("env.{key} must be a string"), None).into());
                    };
                    parsed.insert(key.clone(), value.to_string());
                }
                Some(parsed)
            }
            _ => return Err(RpcError::new(RPC_INVALID_PARAMS, "env must be an object of string values", None).into()),
        };
        if self.state.lock().processes.contains_key(&process_id)
            || self.state.lock().exited_groups.contains_key(&process_id)
        {
            return Err(RpcError::new(RPC_INVALID_REQUEST, format!("duplicate process id {process_id}"), None).into());
        }
        if self.state.lock().terminated_ids.shift_remove(&process_id) {
            return Err(RpcError::new(
                RPC_INVALID_REQUEST,
                format!("process id {process_id} was terminated before it started"),
                None,
            ).into());
        }
        let entry = Arc::new(Mutex::new(ManagedProcess {
            process_id: process_id.clone(),
            state: ProcessState::Starting,
            pid: -1,
            child: None,
            stdin_open: true,
            terminate_after_start: false,
            client_paused: false,
            exit_code: None,
            closed: false,
            open_streams: 2,
            stdin_waiters: Vec::new(),
            kill_timer: None,
        }));
        self.state.lock().processes.insert(process_id.clone(), entry.clone());
        self.start_pipe(entry.clone(), &argv, &cwd, env).await?;
        let terminate = {
            let state = self.state.lock();
            state.disposed || state.shutting_down || entry.lock().terminate_after_start
        };
        if terminate {
            self.state.lock().processes.remove(&process_id);
            self.kill_group(&entry, "SIGKILL");
            return Err(RpcError::new(RPC_INTERNAL_ERROR, "server is shutting down", None).into());
        }
        entry.lock().state = ProcessState::Running;
        if entry.lock().terminate_after_start {
            let entry = entry.clone();
            tokio::spawn(async move {
                let _ = self.begin_termination(&entry).await;
            });
        }
        Ok(ProcessStartResult { pid: entry.lock().pid as u32 })
    }

    async fn start_pipe(
        &self,
        entry: Arc<Mutex<ManagedProcess>>,
        argv: &[String],
        cwd: &str,
        env: Option<HashMap<String, String>>,
    ) -> Result<(), WireAsyncError> {
        let child = match self.host.spawn(&argv[0], &argv[1..], Some(cwd), env.as_ref(), true).await {
            Ok(child) => child,
            Err(error) => {
                self.state.lock().processes.remove(&entry.lock().process_id);
                let mut data = Map::new();
                data.insert("domainCode".to_string(), OS_PROCESS_SPAWN_FAILED.into());
                if let Some(errno) = error.downcast_ref::<crate::os::interface::host_process::HostProcessError>().and_then(|error| error.errno()) {
                    data.insert("errno".to_string(), errno.into());
                }
                return Err(RpcError::new(
                    RPC_INTERNAL_ERROR,
                    format!("failed to spawn \"{}\": {error}", argv[0]),
                    Some(Value::Object(data)),
                ).into());
            }
        };
        let pid = child.pid();
        entry.lock().child = Some(child.clone());
        entry.lock().pid = pid;
        self.apply_output_pause(&entry);
        self.pump_stream(entry.clone(), child.clone(), ProcessOutputStream::Stdout);
        self.pump_stream(entry.clone(), child.clone(), ProcessOutputStream::Stderr);
        let this = self.clone_handle();
        tokio::spawn(async move {
            if let Ok(code) = child.wait().await {
                this.on_exit(&entry, code);
            }
        });
        Ok(())
    }

    fn pump_stream(&self, entry: Arc<Mutex<ManagedProcess>>, child: Arc<dyn IHostProcess>, stream: ProcessOutputStream) {
        let this = self.clone_handle();
        tokio::spawn(async move {
            let mut output = match stream {
                ProcessOutputStream::Stdout => child.stdout(),
                ProcessOutputStream::Stderr => child.stderr(),
            };
            use futures_util::StreamExt;
            while let Some(chunk) = output.next().await {
                match chunk {
                    Ok(chunk) => this.pump(&entry, stream, &chunk),
                    Err(_) => break,
                }
            }
            this.on_stream_end(&entry);
        });
    }

    fn pump(&self, entry: &Arc<Mutex<ManagedProcess>>, stream: ProcessOutputStream, chunk: &[u8]) {
        let entry_snapshot = entry.lock();
        if self.state.lock().disposed {
            return;
        }
        let current = self.state.lock().processes.get(&entry_snapshot.process_id).cloned();
        if current.is_none_or(|current| !Arc::ptr_eq(&current, entry)) {
            return;
        }
        (self.notify)(
            PROCESS_OUTPUT_METHOD,
            json!({
                "processId": entry_snapshot.process_id,
                "stream": match stream { ProcessOutputStream::Stdout => "stdout", ProcessOutputStream::Stderr => "stderr" },
                "chunkBase64": base64::engine::general_purpose::STANDARD.encode(chunk),
            }),
        );
    }

    fn on_stream_end(&self, entry: &Arc<Mutex<ManagedProcess>>) {
        {
            let mut entry = entry.lock();
            if entry.open_streams > 0 {
                entry.open_streams -= 1;
            }
        }
        self.maybe_close(entry);
    }

    fn on_exit(&self, entry: &Arc<Mutex<ManagedProcess>>, exit_code: i32) {
        {
            let mut entry = entry.lock();
            let current = self.state.lock().processes.get(&entry.process_id).cloned();
            if current.is_none_or(|current| !Arc::ptr_eq(&current, entry)) || entry.exit_code.is_some() {
                return;
            }
            entry.exit_code = Some(exit_code);
        }
        self.break_stdin(entry);
        (self.notify)(PROCESS_EXITED_METHOD, json!({ "processId": entry.lock().process_id, "exitCode": exit_code }));
        self.maybe_close(entry);
    }

    fn break_stdin(&self, entry: &Arc<Mutex<ManagedProcess>>) {
        let waiters = {
            let mut entry = entry.lock();
            entry.stdin_open = false;
            std::mem::take(&mut entry.stdin_waiters)
        };
        for waiter in waiters {
            waiter.notify_waiters();
        }
    }

    fn maybe_close(&self, entry: &Arc<Mutex<ManagedProcess>>) {
        let pid = {
            let mut entry = entry.lock();
            if self.state.lock().disposed || entry.closed || entry.exit_code.is_none() || entry.open_streams != 0 {
                return;
            }
            entry.closed = true;
            (self.notify)(PROCESS_CLOSED_METHOD, json!({ "processId": entry.process_id }));
            entry.pid
        };
        let (process_id, kill_timer) = {
            let mut entry = entry.lock();
            let process_id = entry.process_id.clone();
            self.state.lock().processes.remove(&process_id);
            (process_id, entry.kill_timer.take())
        };
        if cfg!(windows) {
            if let Some(timer) = kill_timer {
                timer.abort();
            }
            return;
        }
        let serial = {
            let mut state = self.state.lock();
            state.next_exited_group_serial += 1;
            state.next_exited_group_serial
        };
        let group = ExitedProcessGroup {
            serial,
            pid,
            kill_timer,
            expiry_timer: None,
        };
        self.state.lock().exited_groups.insert(process_id.clone(), group);
        self.schedule_group_reap(process_id, serial);
    }

    fn schedule_group_reap(&self, process_id: String, serial: u64) {
        let this = self.clone_handle();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(GROUP_REAP_INTERVAL_MS)).await;
            let pid = match this.state.lock().exited_groups.get(&process_id) {
                Some(group) if group.serial == serial => group.pid,
                _ => return,
            };
            if kill_process_group(pid, "0") {
                let mut state = this.state.lock();
                if state.exited_groups.get(&process_id).is_some_and(|group| group.serial == serial) {
                    if let Some(group) = state.exited_groups.get_mut(&process_id) {
                        if let Some(timer) = group.kill_timer.take() {
                            timer.abort();
                        }
                    }
                    state.exited_groups.remove(&process_id);
                }
                return;
            }
            this.schedule_group_reap(process_id, serial);
        });
        if let Some(group) = self.state.lock().exited_groups.get_mut(&process_id)
            && group.serial == serial
        {
            group.expiry_timer = Some(timer);
        } else {
            timer.abort();
        }
    }

    pub async fn write(&self, raw_params: Value) -> Result<Value, WireAsyncError> {
        let params = require_params(raw_params)?;
        let process_id = require_string(&params, "processId")?;
        let chunk_base64 = params.get("chunkBase64").and_then(Value::as_str).ok_or_else(|| {
            RpcError::new(RPC_INVALID_PARAMS, "chunkBase64 must be a string", None)
        })?;
        let eof = optional_boolean(&params, "eof")?.unwrap_or(false);
        if eof && !chunk_base64.is_empty() {
            return Err(RpcError::new(RPC_INVALID_PARAMS, "eof writes must carry an empty chunk", None).into());
        }
        let entry = self.state.lock().processes.get(&process_id).cloned();
        let Some(entry) = entry else {
            return Ok(Value::Object(Map::new()));
        };
        if !entry.lock().stdin_open {
            return Ok(Value::Object(Map::new()));
        }
        if eof {
            entry.lock().stdin_open = false;
            if let Some(child) = &entry.lock().child {
                child.stdin_end().await?;
            }
            return Ok(Value::Object(Map::new()));
        }
        let chunk = crate::remote::protocol::codec::decode_base64(chunk_base64);
        if !chunk.is_empty() {
            let child = entry.lock().child.clone();
            let Some(child) = child else {
                entry.lock().stdin_open = false;
                return Ok(Value::Object(Map::new()));
            };
            if !child.stdin_write(&chunk).await? {
                let waiter = Arc::new(tokio::sync::Notify::new());
                let notified = waiter.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                entry.lock().stdin_waiters.push(waiter.clone());
                tokio::select! {
                    _ = child.stdin_drain() => {}
                    _ = notified => {}
                }
                entry
                    .lock()
                    .stdin_waiters
                    .retain(|candidate| !Arc::ptr_eq(candidate, &waiter));
            }
        }
        Ok(Value::Object(Map::new()))
    }

    pub async fn signal(&self, raw_params: Value) -> Result<Value, WireAsyncError> {
        let params = require_params(raw_params)?;
        let process_id = require_string(&params, "processId")?;
        let signal = params.get("signal").and_then(Value::as_str).unwrap_or("");
        if !matches!(signal, "interrupt" | "terminate" | "kill") {
            return Err(RpcError::new(RPC_INVALID_PARAMS, "signal must be interrupt, terminate or kill", None).into());
        }
        let entry = self.state.lock().processes.get(&process_id).cloned();
        let Some(entry) = entry else {
            let group = self.state.lock().exited_groups.get_mut(&process_id);
            if let Some(group) = group {
                if signal == "terminate" {
                    kill_process_group(group.pid, "SIGTERM");
                    if group.kill_timer.is_none() {
                        let pid = group.pid;
                        let this = self.clone_handle();
                        let process_id = process_id.clone();
                        group.kill_timer = Some(tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(PROCESS_TERMINATION_GRACE_MS)).await;
                            kill_process_group(pid, "SIGKILL");
                            if let Some(group) = this.state.lock().exited_groups.get_mut(&process_id) {
                                group.kill_timer = None;
                            }
                        }));
                    }
                } else {
                    kill_process_group(group.pid, if signal == "interrupt" { "SIGINT" } else { "SIGKILL" });
                }
                return Ok(Value::Object(Map::new()));
            }
            if signal == "terminate" {
                let mut state = self.state.lock();
                state.terminated_ids.insert(process_id.clone());
                while state.terminated_ids.len() > TERMINATED_ID_CACHE_SIZE {
                    if let Some(oldest) = state.terminated_ids.first().cloned() {
                        state.terminated_ids.shift_remove(&oldest);
                    }
                }
                return Ok(Value::Object(Map::new()));
            }
            return Err(RpcError::new(RPC_INVALID_REQUEST, format!("unknown process id {process_id}"), None).into());
        };
        if entry.lock().state != ProcessState::Running {
            if signal == "terminate" {
                entry.lock().terminate_after_start = true;
                return Ok(Value::Object(Map::new()));
            }
            return Err(RpcError::new(RPC_INVALID_REQUEST, format!("process id {process_id} is starting"), None).into());
        }
        if signal == "terminate" {
            self.begin_termination(&entry).await?;
            return Ok(Value::Object(Map::new()));
        }
        if let Some(child) = &entry.lock().child {
            child.kill(if signal == "interrupt" { "SIGINT" } else { "SIGKILL" }).await?;
        }
        Ok(Value::Object(Map::new()))
    }

    async fn begin_termination(&self, entry: &Arc<Mutex<ManagedProcess>>) -> Result<(), WireAsyncError> {
        if let Some(child) = &entry.lock().child {
            child.kill("SIGTERM").await?;
        }
        if cfg!(windows) {
            return Ok(());
        }
        let this = self.clone_handle();
        let entry = entry.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(PROCESS_TERMINATION_GRACE_MS)).await;
            this.kill_group(&entry, "SIGKILL");
        });
        entry.lock().kill_timer = Some(timer);
        Ok(())
    }

    fn kill_group(&self, entry: &Arc<Mutex<ManagedProcess>>, signal: &str) {
        if let Some(child) = &entry.lock().child {
            let child = child.clone();
            let signal = signal.to_string();
            tokio::spawn(async move {
                let _ = child.kill(&signal).await;
            });
        }
    }

    pub fn set_output_paused(&self, paused: bool) {
        self.state.lock().output_paused = paused;
        for entry in self.state.lock().processes.values().cloned().collect::<Vec<_>>() {
            self.apply_output_pause(&entry);
        }
    }

    pub fn set_client_paused(&self, process_id: &str, paused: bool) {
        let entry = self.state.lock().processes.get(process_id).cloned();
        let Some(entry) = entry else {
            return;
        };
        if entry.lock().client_paused == paused {
            return;
        }
        entry.lock().client_paused = paused;
        self.apply_output_pause(&entry);
    }

    fn apply_output_pause(&self, entry: &Arc<Mutex<ManagedProcess>>) {
        let paused = self.state.lock().output_paused || entry.lock().client_paused;
        if let Some(child) = &entry.lock().child {
            child.set_output_paused(paused);
        }
    }

    pub async fn terminate_all(&self) {
        {
            let state = self.state.lock();
            if state.disposed || state.shutting_down {
                return;
            }
        }
        self.state.lock().shutting_down = true;
        let entries = self.state.lock().processes.values().cloned().collect::<Vec<_>>();
        for entry in &entries {
            if entry.lock().state == ProcessState::Starting {
                entry.lock().terminate_after_start = true;
            }
        }
        for entry in &entries {
            self.kill_group(entry, "SIGTERM");
        }
        let orphaned = self.state.lock().exited_groups.values().map(|group| group.pid).collect::<Vec<_>>();
        for pid in &orphaned {
            kill_process_group(*pid, "SIGTERM");
        }
        if entries.iter().any(|entry| entry.lock().exit_code.is_none()) {
            let deadline = chrono::Utc::now().timestamp_millis() + PROCESS_TERMINATION_GRACE_MS as i64;
            while chrono::Utc::now().timestamp_millis() < deadline {
                if entries.iter().all(|entry| entry.lock().exit_code.is_some()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        if !cfg!(windows) {
            for entry in &entries {
                self.kill_group(entry, "SIGKILL");
            }
        }
        let mut groups = orphaned;
        groups.extend(self.state.lock().exited_groups.values().map(|group| group.pid));
        for pid in groups {
            kill_process_group(pid, "SIGKILL");
        }
        self.dispose();
    }

    pub fn dispose(&self) {
        let mut state = self.state.lock();
        if state.disposed {
            return;
        }
        state.disposed = true;
        for entry in state.processes.values() {
            if let Some(timer) = entry.lock().kill_timer.take() {
                timer.abort();
            }
            let waiters = {
                let mut entry = entry.lock();
                entry.stdin_open = false;
                std::mem::take(&mut entry.stdin_waiters)
            };
            for waiter in waiters {
                waiter.notify_waiters();
            }
        }
        state.processes.clear();
        state.terminated_ids.clear();
        for group in state.exited_groups.values_mut() {
            if let Some(timer) = group.kill_timer.take() {
                timer.abort();
            }
            if let Some(timer) = group.expiry_timer.take() {
                timer.abort();
            }
        }
        state.exited_groups.clear();
    }

    fn clone_handle(&self) -> Self {
        Self {
            notify: self.notify.clone(),
            host: self.host.clone(),
            state: self.state.clone(),
        }
    }
}

#[cfg(unix)]
fn kill_process_group(pid: i32, signal: &str) -> bool {
    let signal = match signal {
        "SIGTERM" => libc::SIGTERM,
        "SIGINT" => libc::SIGINT,
        "SIGKILL" => libc::SIGKILL,
        "0" => 0,
        _ => return false,
    };
    let result = unsafe { libc::kill(-pid, signal) };
    if signal == 0 {
        return result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(windows)]
fn kill_process_group(_pid: i32, _signal: &str) -> bool {
    false
}
