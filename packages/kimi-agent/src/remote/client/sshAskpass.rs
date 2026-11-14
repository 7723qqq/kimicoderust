use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::remote::askpass::{SSH_ASKPASS_PORT_ENV, SSH_ASKPASS_TOKEN_ENV};

use super::launchers::sh_quote;

#[derive(Clone)]
pub struct SshAuthOptions {
    pub password: String,
    pub command: Vec<String>,
}

pub struct SshAskpass {
    pub env: HashMap<String, String>,
    state: Arc<Mutex<AskpassState>>,
}

struct AskpassState {
    closed: bool,
    sockets: HashMap<usize, Arc<tokio::sync::Mutex<tokio::net::TcpStream>>>,
    directory: Option<String>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

impl SshAskpass {
    pub async fn close(&self) {
        let (sockets, directory, task) = {
            let mut state = self.state.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            (
                state.sockets.clone(),
                state.directory.clone(),
                state.accept_task.take(),
            )
        };
        if let Some(task) = task {
            task.abort();
        }
        for (_, socket) in sockets {
            let _ = socket.lock().await.shutdown().await;
        }
        if let Some(directory) = directory {
            let _ = tokio::fs::remove_dir_all(directory).await;
        }
    }
}

pub async fn create_ssh_askpass(options: SshAuthOptions) -> Result<SshAskpass, std::io::Error> {
    let token = uuid::Uuid::new_v4().to_string();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let state = Arc::new(Mutex::new(AskpassState {
        closed: false,
        sockets: HashMap::new(),
        directory: None,
        accept_task: None,
    }));
    let password = options.password.clone();
    let accept_state = state.clone();
    let accept_token = token.clone();
    let accept_task = tokio::spawn(async move {
        let mut next_id = 0usize;
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(_) => {
                    let sockets = accept_state.lock().sockets.clone();
                    for (_, socket) in sockets {
                        let _ = socket.lock().await.shutdown().await;
                    }
                    break;
                }
            };
            let socket = Arc::new(tokio::sync::Mutex::new(socket));
            let id = next_id;
            next_id += 1;
            accept_state.lock().sockets.insert(id, socket.clone());
            let token = accept_token.clone();
            let password = password.clone();
            let socket_state = accept_state.clone();
            tokio::spawn(async move {
                let mut socket = socket.lock().await;
                let mut input = Vec::new();
                let mut buffer = [0u8; 1024];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(read) => {
                            input.extend_from_slice(&buffer[..read]);
                            if input.len() > 4096 {
                                break;
                            }
                            if let Some(end) = input.iter().position(|byte| *byte == b'\n') {
                                let request = serde_json::from_slice::<serde_json::Value>(&input[..end]);
                                if request
                                    .ok()
                                    .and_then(|request| request.get("token").and_then(|value| value.as_str()).map(str::to_string))
                                    .as_deref()
                                    == Some(token.as_str())
                                {
                                    let _ = socket
                                        .write_all(format!("{}\n", json!({ "value": password })).as_bytes())
                                        .await;
                                    let _ = socket.shutdown().await;
                                }
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                socket_state.lock().sockets.remove(&id);
            });
        }
    });
    state.lock().accept_task = Some(accept_task);
    let mut binary = options.command[0].clone();
    let setup = async {
        if options.command.len() > 1 && !cfg!(windows) {
            let base = std::env::temp_dir().join(format!("kimi-ssh-askpass-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir(&base).await?;
            state.lock().directory = Some(base.to_string_lossy().into_owned());
            binary = base.join("askpass").to_string_lossy().into_owned();
            let script = format!(
                "#!/bin/sh\nexec {} \"$@\"\n",
                options
                    .command
                    .iter()
                    .map(|part| sh_quote(part))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            tokio::fs::write(&binary, script).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut permissions = tokio::fs::metadata(&binary).await?.permissions();
                permissions.set_mode(0o700);
                tokio::fs::set_permissions(&binary, permissions).await?;
            }
        }
        Ok::<(), std::io::Error>(())
    }
    .await;
    if let Err(error) = setup {
        SshAskpass {
            env: HashMap::new(),
            state: state.clone(),
        }
        .close()
        .await;
        return Err(error);
    }
    Ok(SshAskpass {
        env: HashMap::from([
            ("SSH_ASKPASS".to_string(), binary),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            (
                "DISPLAY".to_string(),
                std::env::var("DISPLAY")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "kimi".to_string()),
            ),
            (SSH_ASKPASS_PORT_ENV.to_string(), address.port().to_string()),
            (SSH_ASKPASS_TOKEN_ENV.to_string(), token),
        ]),
        state,
    })
}
