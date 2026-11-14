use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, mpsc};

use crate::events::EngineEvent;
use crate::server::hub::EventHub;

const DEFAULT_COLS: u32 = 80;
const DEFAULT_ROWS: u32 = 24;
const MAX_BUFFER_FRAMES: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalDescriptor {
    pub id: String,
    pub session_id: String,
    pub cwd: String,
    pub shell: String,
    pub cols: u32,
    pub rows: u32,
    pub status: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

struct TerminalEntry {
    descriptor: TerminalDescriptor,
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    buffer: Vec<String>,

    next_seq: usize,

    dropped: usize,

    master: Mutex<Option<Box<dyn MasterPty + Send>>>,

    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,

    #[cfg(windows)]
    child_pid: Option<u32>,

    dead: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct TerminalManager {
    entries: Arc<RwLock<HashMap<String, TerminalEntry>>>,
    hub: Arc<EventHub>,
}

type PtyEndpoints = (Box<dyn Write + Send>, Box<dyn Read + Send>);

impl TerminalManager {
    pub fn new(hub: Arc<EventHub>) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            hub,
        }
    }

    pub fn default_shell() -> String {
        if let Ok(shell) = std::env::var("KIMI_SHELL_PATH")
            && !shell.trim().is_empty()
        {
            return shell.trim().to_string();
        }
        #[cfg(windows)]
        {
            let ps_path = std::path::Path::new(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
            );
            if ps_path.exists() {
                return ps_path.to_string_lossy().to_string();
            }
            "powershell.exe".to_string()
        }
        #[cfg(not(windows))]
        {
            if let Ok(shell) = std::env::var("SHELL")
                && !shell.trim().is_empty()
            {
                return shell.trim().to_string();
            }
            "/bin/sh".to_string()
        }
    }

    fn take_pty_endpoints(
        master: &mut dyn MasterPty,
        killer: &mut dyn ChildKiller,
    ) -> Result<PtyEndpoints, String> {
        let writer = match master.take_writer() {
            Ok(writer) => writer,
            Err(e) => {
                let _ = killer.kill();
                return Err(format!("Failed to take pty writer: {e}"));
            }
        };
        match master.try_clone_reader() {
            Ok(reader) => Ok((writer, reader)),
            Err(e) => {
                let _ = killer.kill();
                Err(format!("Failed to take pty reader: {e}"))
            }
        }
    }

    pub async fn create(
        &self,
        session_id: &str,
        cwd: &str,
        shell_opt: Option<&str>,
        cols_opt: Option<u32>,
        rows_opt: Option<u32>,
    ) -> Result<TerminalDescriptor, String> {
        let term_id = format!("term_{}", fastrand::u64(..));
        let shell = shell_opt
            .map(|s| s.to_string())
            .unwrap_or_else(Self::default_shell);
        let cols = cols_opt.unwrap_or(DEFAULT_COLS);
        let rows = rows_opt.unwrap_or(DEFAULT_ROWS);
        let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let pty_system = native_pty_system();
        let mut pair = pty_system
            .openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to open pty for '{shell}': {e}"))?;

        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(Path::new(cwd));

        cmd.env("TERM", "xterm-256color");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to spawn shell '{shell}': {e}"))?;
        #[cfg(windows)]
        let child_pid = child.process_id();

        let mut killer = child.clone_killer();

        let (writer, reader) = Self::take_pty_endpoints(pair.master.as_mut(), killer.as_mut())?;

        let master: Box<dyn MasterPty + Send> = pair.master;

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(128);

        let descriptor = TerminalDescriptor {
            id: term_id.clone(),
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            shell,
            cols,
            rows,
            status: "running".to_string(),
            created_at,
            exited_at: None,
            exit_code: None,
        };

        let dead = Arc::new(AtomicBool::new(false));

        let entry = TerminalEntry {
            descriptor: descriptor.clone(),
            stdin_tx: Some(stdin_tx),
            buffer: Vec::new(),
            next_seq: 0,
            dropped: 0,
            master: Mutex::new(Some(master)),
            killer: Mutex::new(Some(killer)),
            #[cfg(windows)]
            child_pid,
            dead: dead.clone(),
        };

        {
            let mut lock = self.entries.write().await;
            lock.insert(term_id.clone(), entry);
        }

        {
            let mut writer = writer;
            let mut stdin_rx = stdin_rx;
            std::thread::spawn(move || {
                while let Some(bytes) = stdin_rx.blocking_recv() {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            });
        }

        {
            let entries_out = self.entries.clone();
            let hub_out = self.hub.clone();
            let tid = term_id.clone();
            let sid = session_id.to_string();
            let dead_out = dead.clone();
            let mut reader = reader;
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    if dead_out.load(Ordering::SeqCst) {
                        break;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]).to_string();
                            let out_seq = {
                                let mut lock = entries_out.blocking_write();
                                if let Some(e) = lock.get_mut(&tid) {
                                    e.next_seq += 1;
                                    let seq = e.next_seq;
                                    if e.buffer.len() >= MAX_BUFFER_FRAMES {
                                        e.buffer.remove(0);
                                        e.dropped += 1;
                                    }
                                    e.buffer.push(text.clone());
                                    seq
                                } else {
                                    0
                                }
                            };

                            let out_event = json!({
                                "type": "terminal_output",
                                "session_id": sid,
                                "terminal_id": tid,
                                "seq": out_seq,
                                "data": text,
                            });
                            hub_out
                                .bus_for(&sid)
                                .publish(&EngineEvent::Custom(out_event));
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        {
            let entries_exit = self.entries.clone();
            let hub_exit = self.hub.clone();
            let tid = term_id.clone();
            let sid = session_id.to_string();
            let dead_exit = dead.clone();
            std::thread::spawn(move || {
                let status = child.wait();
                let exit_code = status.ok().map(|s| s.exit_code() as i32);
                dead_exit.store(true, Ordering::SeqCst);
                let exited_at =
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

                {
                    let mut lock = entries_exit.blocking_write();
                    if let Some(e) = lock.get_mut(&tid) {
                        e.descriptor.status = "exited".to_string();
                        e.descriptor.exited_at = Some(exited_at.clone());
                        e.descriptor.exit_code = exit_code;
                        e.stdin_tx = None;
                    }
                }

                let exit_event = json!({
                    "type": "terminal_exit",
                    "session_id": sid,
                    "terminal_id": tid,
                    "payload": { "exit_code": exit_code },
                });
                hub_exit
                    .bus_for(&sid)
                    .publish(&EngineEvent::Custom(exit_event));
            });
        }

        Ok(descriptor)
    }

    pub async fn list(&self, session_id: &str) -> Vec<TerminalDescriptor> {
        let lock = self.entries.read().await;
        lock.values()
            .filter(|e| e.descriptor.session_id == session_id)
            .map(|e| e.descriptor.clone())
            .collect()
    }

    pub async fn get(&self, session_id: &str, terminal_id: &str) -> Option<TerminalDescriptor> {
        let lock = self.entries.read().await;
        let entry = lock.get(terminal_id)?;
        if entry.descriptor.session_id == session_id {
            Some(entry.descriptor.clone())
        } else {
            None
        }
    }

    pub async fn write(
        &self,
        session_id: &str,
        terminal_id: &str,
        data: &[u8],
    ) -> Result<(), String> {
        let lock = self.entries.read().await;
        let entry = lock
            .get(terminal_id)
            .ok_or_else(|| "Terminal not found".to_string())?;
        if entry.descriptor.session_id != session_id {
            return Err("Terminal not found in session".to_string());
        }
        if let Some(ref tx) = entry.stdin_tx {
            tx.send(data.to_vec())
                .await
                .map_err(|e| format!("Failed to send input: {e}"))?;
            Ok(())
        } else {
            Err("Terminal is not running".to_string())
        }
    }

    pub async fn resize(
        &self,
        session_id: &str,
        terminal_id: &str,
        cols: u32,
        rows: u32,
    ) -> Result<(), String> {
        let mut lock = self.entries.write().await;
        let entry = lock
            .get_mut(terminal_id)
            .ok_or_else(|| "Terminal not found".to_string())?;
        if entry.descriptor.session_id != session_id {
            return Err("Terminal not found in session".to_string());
        }

        if let Some(ref mut master) = *entry.master.lock().unwrap() {
            let _ = master.resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        entry.descriptor.cols = cols;
        entry.descriptor.rows = rows;
        Ok(())
    }

    pub async fn close_all(&self) -> usize {
        let running: Vec<(String, String)> = {
            let lock = self.entries.read().await;
            lock.values()
                .filter(|entry| entry.descriptor.status == "running")
                .map(|entry| {
                    (
                        entry.descriptor.session_id.clone(),
                        entry.descriptor.id.clone(),
                    )
                })
                .collect()
        };
        let mut closed = 0;
        for (session_id, terminal_id) in running {
            match self.close(&session_id, &terminal_id).await {
                Ok(()) => closed += 1,
                Err(error) => tracing::warn!(
                    session_id,
                    terminal_id,
                    %error,
                    "shutdown could not close a terminal"
                ),
            }
        }
        closed
    }

    pub async fn close(&self, session_id: &str, terminal_id: &str) -> Result<(), String> {
        let mut lock = self.entries.write().await;
        let entry = lock
            .get_mut(terminal_id)
            .ok_or_else(|| "Terminal not found".to_string())?;
        if entry.descriptor.session_id != session_id {
            return Err("Terminal not found in session".to_string());
        }
        if entry.descriptor.status == "running" {
            entry.descriptor.status = "exited".to_string();
            entry.descriptor.exited_at =
                Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));

            entry.stdin_tx = None;

            let killer = entry.killer.lock().unwrap().take();
            #[cfg(windows)]
            let pid = entry.child_pid;

            entry.dead.store(true, Ordering::SeqCst);
            drop(lock);

            if let Some(mut k) = killer {
                let _ = k.kill();
            }

            #[cfg(windows)]
            if let Some(pid) = pid {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .output();
            }
        }
        Ok(())
    }

    pub async fn output(
        &self,
        session_id: &str,
        terminal_id: &str,
        since_seq: usize,
    ) -> Result<(Vec<String>, usize), String> {
        let lock = self.entries.read().await;
        let entry = lock
            .get(terminal_id)
            .ok_or_else(|| "Terminal not found".to_string())?;
        if entry.descriptor.session_id != session_id {
            return Err("Terminal not found in session".to_string());
        }

        let start = since_seq.saturating_sub(entry.dropped);
        let start = start.min(entry.buffer.len());
        Ok((entry.buffer[start..].to_vec(), entry.next_seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_terminal_manager_lifecycle() {
        let hub = Arc::new(EventHub::new());
        let mgr = TerminalManager::new(hub);
        let temp_dir = tempfile::tempdir().unwrap();
        let cwd = temp_dir.path().to_string_lossy().to_string();

        let desc = mgr
            .create("sess-1", &cwd, None, Some(80), Some(24))
            .await
            .unwrap();
        assert_eq!(desc.session_id, "sess-1");
        assert_eq!(desc.status, "running");
        assert_eq!(desc.cols, 80);
        assert_eq!(desc.rows, 24);
        assert!(desc.id.starts_with("term_"));

        let list = mgr.list("sess-1").await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, desc.id);

        assert!(mgr.list("sess-other").await.is_empty());

        let fetched = mgr.get("sess-1", &desc.id).await.unwrap();
        assert_eq!(fetched.id, desc.id);

        assert!(mgr.get("sess-1", "term_none").await.is_none());

        assert!(mgr.get("sess-wrong", &desc.id).await.is_none());

        mgr.resize("sess-1", &desc.id, 120, 30).await.unwrap();
        let resized = mgr.get("sess-1", &desc.id).await.unwrap();
        assert_eq!(resized.cols, 120);
        assert_eq!(resized.rows, 30);

        assert_eq!(
            mgr.resize("sess-1", "term_none", 100, 20)
                .await
                .unwrap_err(),
            "Terminal not found"
        );

        mgr.write("sess-1", &desc.id, b"echo test\n")
            .await
            .expect("terminal write must succeed");

        let (frames, total) = mgr
            .output("sess-1", &desc.id, 0)
            .await
            .expect("output query succeeds");
        assert!(total >= frames.len());

        let (empty_frames, total2) = mgr.output("sess-1", &desc.id, 100_000).await.unwrap();
        assert!(empty_frames.is_empty());
        assert_eq!(total, total2);

        assert!(mgr.output("sess-1", "term_none", 0).await.is_err());
        assert!(mgr.output("sess-wrong", &desc.id, 0).await.is_err());

        mgr.close("sess-1", &desc.id).await.unwrap();
        let closed = mgr.get("sess-1", &desc.id).await.unwrap();
        assert_eq!(closed.status, "exited");
        assert!(closed.exited_at.is_some());

        assert_eq!(
            mgr.write("sess-1", &desc.id, b"ls\n").await.unwrap_err(),
            "Terminal is not running"
        );
    }

    #[test]
    fn test_terminal_default_shell_resolution() {
        let shell = TerminalManager::default_shell();
        assert!(!shell.trim().is_empty());
        #[cfg(windows)]
        {
            assert!(
                shell.ends_with(".exe")
                    || shell.contains("powershell")
                    || shell.contains("cmd")
                    || shell.contains("bash")
            );
        }
    }
}
