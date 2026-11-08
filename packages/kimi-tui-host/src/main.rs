//! `kimi-tui` — the terminal UI against a live engine session.
//!
//! Startup order matters: the engine's runtime comes up first (it owns tokio),
//! then the terminal is taken over by textual-rs on the main thread. The two
//! talk over one channel each way, so neither blocks the other.
//!
//! Keys: Enter sends the prompt, Esc or Ctrl-C stops the turn in flight, and a
//! double Ctrl-C quits (textual-rs's own contract); Ctrl-D quits immediately.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use kimi_agent::config::KimiConfig;
use kimi_tui_app::run_terminal;
use kimi_ui_core::{EntryContent, EntryId, EntryKind, EntryOp};

fn main() -> Result<()> {
    let (workspace, model, profile) = parse_args()?;

    let (config, config_path) = KimiConfig::discover().map_err(|error| anyhow!(error))?;
    let workspace = workspace.canonicalize().unwrap_or(workspace);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let built = runtime.block_on(kimi_tui_host::build_engine(
        config,
        workspace.clone(),
        model,
        profile,
    ))?;

    banner(
        &built.state,
        &config_path,
        &workspace,
        built.permission_mode,
    );

    let session = built.session.clone();
    let bridge = std::sync::Arc::clone(&built.bridge);
    let state = std::sync::Arc::clone(&built.state);
    runtime.spawn(kimi_tui_host::drive_commands(
        session.clone(),
        built.commands,
        bridge,
        state,
        built.steer_trace.clone(),
    ));

    let ui = run_terminal(built.state);
    session.shutdown();
    ui
}

/// One banner entry so the first frame says where the session is running — and
/// what the session will do with a permission question it cannot ask.
fn banner(
    state: &kimi_tui_app::UiState,
    config: &std::path::Path,
    workspace: &std::path::Path,
    permission_mode: kimi_agent::permission::PermissionMode,
) {
    let text = format!(
        "kimi-tui · {}\nconfig: {}\napprovals: {} (no approval dialog yet — every request the policy asks about is allowed)\nEnter sends · Esc / Ctrl-C stops the turn · Ctrl-D quits",
        workspace.display(),
        config.display(),
        match permission_mode {
            kimi_agent::permission::PermissionMode::Yolo => "yolo",
            kimi_agent::permission::PermissionMode::Auto => "auto",
            _ => "manual",
        }
    );
    let id = EntryId::engine("banner", 0, EntryKind::Banner);
    state.apply(EntryOp::Upsert {
        id,
        content: EntryContent::new(EntryKind::Banner, text),
    });
}

/// `kimi-tui [--workspace <dir>] [--model <alias>] [--agent <profile>]`
fn parse_args() -> Result<(PathBuf, Option<String>, Option<String>)> {
    let mut workspace = std::env::current_dir()?;
    let mut model = None;
    let mut profile = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" | "-C" => {
                workspace = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow!("--workspace needs a directory"))?;
            }
            "--model" | "-m" => {
                model = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--model needs an alias"))?,
                );
            }
            "--agent" | "-a" => {
                profile = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--agent needs a profile name"))?,
                );
            }
            "--help" | "-h" => {
                println!("kimi-tui [--workspace <dir>] [--model <alias>] [--agent <profile>]");
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    Ok((workspace, model, profile))
}
