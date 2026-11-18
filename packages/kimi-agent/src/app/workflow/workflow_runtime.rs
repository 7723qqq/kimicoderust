use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use rquickjs::function::Async;
use rquickjs::{Array, AsyncContext, AsyncRuntime, Ctx, Function, Object, Promise, Value as JsValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, oneshot};

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::{AbortController, AbortError, AbortSignal, abortable, link_abort_signal};
use crate::agent::profile::profile::{
    AgentProfileService, BindAgentInput, I_AGENT_PROFILE_SERVICE,
};
use crate::app::auth::web_search::web_search::{WebSearchOptions, WebSearchProviderService};
use crate::i18n::{LocalizedText, i18n_params};
use crate::os::interface::hostTimers::{IHostTimerHandle, IHostTimers};
use crate::session::agent_lifecycle::agent_lifecycle::{CreateAgentOptions, IAgentLifecycleService};
use crate::session::session_context::session_context::ISessionContext;
use crate::session::subagent::subagent::{AgentRunRequest, ISessionSubagentService, RunAgentOptions};
use crate::wire::record::WireAsyncError;

use super::workflow_types::{AgentOpts, SharedWorkflowRunEntry, WorkflowRunEntry};

static MAX_CONCURRENT: LazyLock<usize> = LazyLock::new(|| {
    let parallelism = std::thread::available_parallelism().map(|value| value.get()).unwrap_or(4);
    16.min(2 * parallelism)
});

const GLOB_WALK_IGNORED_DIRS: &[&str] = &["node_modules", ".git"];
const MAX_GLOB_WALK_DEPTH: usize = 8;
const MAX_GLOB_RESULTS: usize = 1000;

pub const WORKFLOW_DEADLINE_DEFAULT_MS: u64 = 30 * 60 * 1000;
pub const WORKFLOW_SYNC_PREFIX_LIMIT_MS: u64 = 30_000;
pub const WORKFLOW_DEADLINE_ERROR_MESSAGE: &str = "Workflow script deadline exceeded";
pub const WORKFLOW_CANCELLED_ERROR_MESSAGE: &str = "Workflow cancelled";

#[derive(Default)]
struct SystemHostTimers {
    next_handle: AtomicU64,
    tasks: Mutex<HashMap<IHostTimerHandle, tokio::task::JoinHandle<()>>>,
}

impl SystemHostTimers {
    fn start(&self, callback: Arc<dyn Fn() + Send + Sync>, timeout_ms: u64) -> IHostTimerHandle {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed) + 1;
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
            callback();
        });
        self.tasks.lock().insert(handle, task);
        handle
    }
}

impl IHostTimers for SystemHostTimers {
    fn set_clamped_timeout(&self, callback: Arc<dyn Fn() + Send + Sync>, timeout_ms: u64) -> IHostTimerHandle {
        self.start(callback, timeout_ms)
    }

    fn set_timeout(&self, callback: Arc<dyn Fn() + Send + Sync>, timeout_ms: u64) -> IHostTimerHandle {
        self.start(callback, timeout_ms)
    }

    fn clear_timeout(&self, handle: IHostTimerHandle) {
        if let Some(task) = self.tasks.lock().remove(&handle) {
            task.abort();
        }
    }
}

static SYSTEM_HOST_TIMERS: LazyLock<Arc<SystemHostTimers>> = LazyLock::new(|| Arc::new(SystemHostTimers::default()));

#[derive(Clone)]
pub struct WorkflowRuntimeDeps {
    pub lifecycle: Arc<dyn IAgentLifecycleService>,
    pub subagents: Arc<dyn ISessionSubagentService>,
    pub session_context: Arc<dyn ISessionContext>,
    pub log: Arc<dyn ILogService>,
    pub caller_agent_id: String,
    pub workspace_root: String,
    pub web_search_provider: Option<Arc<dyn WebSearchProviderService>>,
}

pub struct WorkflowRunOptions {
    pub script: String,
    pub args: Option<Value>,
    pub deps: WorkflowRuntimeDeps,
    pub entry: SharedWorkflowRunEntry,
    pub deadline_ms: Option<u64>,
    pub sync_prefix_limit_ms: Option<u64>,
    pub timers: Option<Arc<dyn IHostTimers>>,
    pub on_deadline_aborted: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowExecOptions {
    pub timeout_ms: Option<u64>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkflowFetchOptions {
    pub method: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowFetchResult {
    pub ok: bool,
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub async fn execute_workflow(opts: WorkflowRunOptions) -> Result<Option<Value>, WireAsyncError> {
    let WorkflowRunOptions {
        script,
        args,
        deps,
        entry,
        deadline_ms,
        sync_prefix_limit_ms,
        timers,
        on_deadline_aborted,
    } = opts;
    let deadline_ms = deadline_ms.unwrap_or(WORKFLOW_DEADLINE_DEFAULT_MS);
    let sync_prefix_limit_ms = sync_prefix_limit_ms.unwrap_or(WORKFLOW_SYNC_PREFIX_LIMIT_MS);
    let timers = timers.unwrap_or_else(|| Arc::clone(&SYSTEM_HOST_TIMERS) as Arc<dyn IHostTimers>);
    let on_deadline_aborted = on_deadline_aborted.unwrap_or_else(|| Arc::new(|| {}));
    let semaphore = Arc::new(Semaphore::new(*MAX_CONCURRENT));
    let runtime = AsyncRuntime::new().map_err(|error| workflow_error(error.to_string()))?;
    let context = AsyncContext::full(&runtime).await.map_err(|error| workflow_error(error.to_string()))?;
    let sync_phase = Arc::new(AtomicBool::new(true));
    let sync_deadline = Instant::now() + Duration::from_millis(deadline_ms.min(sync_prefix_limit_ms));
    let interrupt_phase = sync_phase.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        interrupt_phase.load(Ordering::Acquire) && Instant::now() >= sync_deadline
    }))).await;

    let (deadline_sender, deadline_receiver) = oneshot::channel();
    let deadline_sender = Arc::new(Mutex::new(Some(deadline_sender)));
    let deadline_entry = entry.clone();
    let deadline_callback = on_deadline_aborted.clone();
    let deadline_handle = timers.set_timeout(Arc::new(move || {
        deadline_entry.abort_controller.abort(Some(Arc::new(AbortError::new(WORKFLOW_DEADLINE_ERROR_MESSAGE))));
        deadline_callback();
        if let Some(sender) = deadline_sender.lock().take() {
            let _ = sender.send(());
        }
    }), deadline_ms);

    let parent_signal = entry.abort_controller.signal();
    let abort_controller = Arc::new(AbortController::new());
    let unlink_abort = link_abort_signal(&parent_signal, abort_controller.clone());
    let abort_future = abortable(std::future::pending::<()>(), abort_controller.signal());
    let script = script.replace("export const meta", "const meta");
    let wrapped_script = format!("(async () => {{\n{script}\n}})()");
    let sync_phase_for_context = sync_phase.clone();

    let outcome = rquickjs::async_with!(context => |ctx| {
        if let Err(error) = install_globals(&ctx, deps, entry, semaphore, args.as_ref()) {
            sync_phase_for_context.store(false, Ordering::Release);
            return Err(render_js_error(&ctx, error));
        }
        let promise: Promise = match ctx.eval(wrapped_script) {
            Ok(promise) => promise,
            Err(error) => {
                sync_phase_for_context.store(false, Ordering::Release);
                return Err(render_js_error(&ctx, error));
            }
        };
        sync_phase_for_context.store(false, Ordering::Release);
        let script_future = promise.into_future::<JsValue>();
        tokio::select! {
            result = script_future => match result {
                Ok(value) => Ok(js_to_optional_json(&value)),
                Err(error) => Err(render_js_error(&ctx, error)),
            },
            _ = deadline_receiver => Err(WORKFLOW_DEADLINE_ERROR_MESSAGE.to_string()),
            reason = abort_future => Err(reason.err().map(|error| error.to_string()).unwrap_or_else(|| WORKFLOW_CANCELLED_ERROR_MESSAGE.to_string())),
        }
    }).await;

    timers.clear_timeout(deadline_handle);
    unlink_abort();
    runtime.set_interrupt_handler(None).await;
    let _ = runtime.idle().await;
    drop(context);
    outcome.map_err(workflow_error)
}

fn install_globals<'js>(
    ctx: &Ctx<'js>,
    deps: WorkflowRuntimeDeps,
    entry: SharedWorkflowRunEntry,
    semaphore: Arc<Semaphore>,
    args: Option<&Value>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    globals.set("args", optional_json_to_js(ctx, args)?)?;

    {
        let entry = entry.clone();
        let log = deps.log.clone();
        globals.set("phase", Function::new(ctx.clone(), move |title: String| {
            entry.set_phase(title.clone());
            log.debug("workflow.phase", Some(json!({ "runId": entry.run_id, "phase": title })));
        })?)?;
    }

    {
        let entry = entry.clone();
        let log = deps.log.clone();
        globals.set("log", Function::new(ctx.clone(), move |message: String| {
            log.debug("workflow.log", Some(json!({ "runId": entry.run_id, "message": message })));
        })?)?;
    }

    {
        let deps = deps.clone();
        let entry = entry.clone();
        globals.set("__native_agent", Function::new(ctx.clone(), Async(move |prompt: String, opts_json: String| {
            let deps = deps.clone();
            let entry = entry.clone();
            let semaphore = semaphore.clone();
            async move {
                if prompt.is_empty() {
                    return Ok::<String, rquickjs::Error>("null".to_string());
                }
                let opts = parse_agent_opts(&opts_json);
                let permit = semaphore.acquire().await.map_err(|error| rquickjs::Error::new_from_js_message("workflow", "agent", error.to_string()))?;
                entry.record_agent();
                let label = opts.label.clone();
                let result = spawn_agent(prompt, opts, deps.clone(), entry.clone()).await;
                drop(permit);
                match result {
                    Ok(value) => Ok(json_string(&value)),
                    Err(error) => {
                        deps.log.warn("workflow.agent.failed", Some(json!({ "label": label, "error": error.to_string() })));
                        Ok("null".to_string())
                    }
                }
            }
        }))?)?;
    }

    let workspace_root = PathBuf::from(&deps.workspace_root);

    {
        let root = workspace_root.clone();
        globals.set("readFile", Function::new(ctx.clone(), Async(move |path: String| {
            let root = root.clone();
            async move {
                let resolved = resolve_in_workspace(&root, &path).map_err(|error| rquickjs::Error::new_from_js_message("path", "readFile", error))?;
                tokio::fs::read_to_string(resolved).await.map_err(|error| rquickjs::Error::new_from_js_message("path", "readFile", error.to_string()))
            }
        }))?)?;
    }

    {
        let root = workspace_root.clone();
        globals.set("writeFile", Function::new(ctx.clone(), Async(move |path: String, content: String| {
            let root = root.clone();
            async move {
                let resolved = resolve_in_workspace(&root, &path).map_err(|error| rquickjs::Error::new_from_js_message("path", "writeFile", error))?;
                tokio::fs::write(resolved, content).await.map_err(|error| rquickjs::Error::new_from_js_message("path", "writeFile", error.to_string()))
            }
        }))?)?;
    }

    {
        let root = workspace_root.clone();
        globals.set("glob", Function::new(ctx.clone(), Async(move |pattern: String| {
            let root = root.clone();
            async move {
                tokio::task::spawn_blocking(move || glob_files(&root, &pattern)).await.map_err(|error| rquickjs::Error::new_from_js_message("glob", "glob", error.to_string()))?.map_err(|error| rquickjs::Error::new_from_js_message("glob", "glob", error))
            }
        }))?)?;
    }

    {
        let root = workspace_root.clone();
        globals.set("exists", Function::new(ctx.clone(), Async(move |path: String| {
            let root = root.clone();
            async move {
                let exists = match resolve_in_workspace(&root, &path) {
                    Ok(resolved) => tokio::fs::metadata(resolved).await.is_ok(),
                    Err(_) => false,
                };
                Ok::<bool, rquickjs::Error>(exists)
            }
        }))?)?;
    }

    {
        let root = workspace_root.clone();
        globals.set("__native_exec", Function::new(ctx.clone(), Async(move |command: String, opts_json: String| {
            let root = root.clone();
            async move {
                let opts = parse_json_or_default::<WorkflowExecOptions>(&opts_json);
                let cwd = match opts.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
                    Some(cwd) => resolve_in_workspace(&root, cwd).map_err(|error| rquickjs::Error::new_from_js_message("path", "exec", error))?,
                    None => root,
                };
                let result = run_shell(&command, &cwd, opts.timeout_ms.unwrap_or(30_000)).await;
                Ok::<String, rquickjs::Error>(json_string(&result))
            }
        }))?)?;
    }

    globals.set("__native_fetch", Function::new(ctx.clone(), Async(move |url: String, opts_json: String| async move {
        let opts = parse_json_or_default::<WorkflowFetchOptions>(&opts_json);
        let result = fetch_result(&url, opts).await;
        Ok::<String, rquickjs::Error>(json_string(&result))
    }))?)?;

    {
        let deps = deps.clone();
        let entry = entry.clone();
        globals.set("__native_search", Function::new(ctx.clone(), Async(move |query: String| {
            let deps = deps.clone();
            let entry = entry.clone();
            async move {
                let Some(provider_service) = deps.web_search_provider.clone() else {
                    deps.log.warn("workflow.search.unavailable", Some(json!({ "runId": entry.run_id })));
                    return Ok::<String, rquickjs::Error>("[]".to_string());
                };
                let Some(provider) = provider_service.get_web_search_provider() else {
                    deps.log.warn("workflow.search.unavailable", Some(json!({ "runId": entry.run_id })));
                    return Ok::<String, rquickjs::Error>("[]".to_string());
                };
                match provider.search(&query, WebSearchOptions::default()).await {
                    Ok(results) => {
                        let results = results.into_iter().map(|result| WorkflowSearchResult {
                            title: result.title,
                            url: result.url,
                            snippet: result.snippet,
                        }).collect::<Vec<_>>();
                        Ok(json_string(&results))
                    }
                    Err(error) => {
                        deps.log.warn("workflow.search.failed", Some(json!({ "runId": entry.run_id, "error": error.to_string() })));
                        Ok("[]".to_string())
                    }
                }
            }
        }))?)?;
    }

    let prelude = r#"
globalThis.agent = async (prompt, opts) => {
  if (typeof prompt !== 'string' || prompt.length === 0) return null;
  return JSON.parse(await __native_agent(prompt, opts === undefined ? '' : JSON.stringify(opts)));
};
globalThis.search = async (query) => JSON.parse(await __native_search(query));
globalThis.exec = async (command, opts) => JSON.parse(await __native_exec(command, opts === undefined ? '' : JSON.stringify(opts)));
globalThis.fetch = async (url, opts) => JSON.parse(await __native_fetch(url, opts === undefined ? '' : JSON.stringify(opts)));
globalThis.parallel = (thunks) => Promise.all(thunks.map((t) => Promise.resolve().then(() => t())));
globalThis.pipeline = (items, ...stages) =>
  Promise.all(items.map((item, index) =>
    stages.reduce((acc, stage) => acc.then((prev) => stage(prev, item, index)), Promise.resolve(item))));
globalThis.console = {
  log: (message) => log(message),
  warn: (message) => log(message),
  error: (message) => log(message),
  debug: (message) => log(message),
};
globalThis.URL = class URL {
  constructor(url) {
    const match = url.match(/^(\w+):\/\/([^/]+)(\/[^?#]*)?(\?[^#]*)?(#.*)?$/);
    if (!match) throw new TypeError(`Invalid URL: ${url}`);
    this.protocol = `${match[1]}:`;
    this.hostname = match[2];
    this.pathname = match[3] ?? '/';
    this.search = match[4] ?? '';
    this.hash = match[5] ?? '';
    this.href = url;
  }
  toString() { return this.href; }
};
"#;
    ctx.eval::<(), _>(prelude)?;
    Ok(())
}

async fn spawn_agent(
    prompt: String,
    opts: AgentOpts,
    deps: WorkflowRuntimeDeps,
    entry: SharedWorkflowRunEntry,
) -> Result<Value, WireAsyncError> {
    let full_prompt = match opts.schema.as_ref() {
        Some(schema) => format!(
            "{prompt}\n\nYou MUST respond with a JSON object matching this schema:\n{}\n\nReturn ONLY the JSON object, no markdown fences, no explanation.",
            serde_json::to_string_pretty(schema)?
        ),
        None => prompt,
    };
    let caller_data = deps.lifecycle.handle_of(&deps.caller_agent_id).map(|handle| {
        handle.accessor().get(&I_AGENT_PROFILE_SERVICE).data()
    });
    let model_alias = opts.model.clone().or_else(|| caller_data.as_ref().and_then(|data| data.base.model_alias.clone()));
    let Some(model_alias) = model_alias else {
        return Err(workflow_error(LocalizedText::new("v2Errors.callerAgentNoModel").render()));
    };
    let mut labels = HashMap::from([
        ("parentAgentId".to_string(), deps.caller_agent_id.clone()),
        ("workflowRunId".to_string(), entry.run_id.clone()),
    ]);
    if let Some(label) = opts.label.clone() {
        labels.insert("workflowLabel".to_string(), label);
    }
    let handle = deps.lifecycle.clone().create(Some(CreateAgentOptions {
        binding: Some(BindAgentInput {
            profile: opts.agent_type.clone().unwrap_or_else(|| "coder".to_string()),
            model: Some(model_alias),
            thinking: caller_data.as_ref().map(|data| data.base.thinking_level.clone()),
            strict_thinking: None,
        }),
        labels: Some(labels),
        ..CreateAgentOptions::default()
    })).await?;

    let result = async {
        let controller = Arc::new(AbortController::new());
        let signal = link_signals(&entry.abort_controller.signal(), &controller);
        let run_handle = deps.subagents.run(
            handle.clone(),
            AgentRunRequest::Prompt {
                prompt: full_prompt,
                helper: None,
            },
            RunAgentOptions {
                signal,
                on_ready: None,
            },
        ).await?;
        let completion = run_handle.completion.await.map_err(|error| Box::new(error) as WireAsyncError)?;
        if opts.schema.is_some() {
            Ok(parse_json_result(&completion.summary))
        } else {
            Ok(Value::String(completion.summary))
        }
    }.await;
    let _ = deps.lifecycle.remove(handle).await;
    result
}

fn parse_agent_opts(raw: &str) -> AgentOpts {
    if raw.trim().is_empty() {
        AgentOpts::default()
    } else {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

fn parse_json_or_default<T>(raw: &str) -> T
where
    T: for<'de> Deserialize<'de> + Default,
{
    if raw.trim().is_empty() {
        T::default()
    } else {
        serde_json::from_str(raw).unwrap_or_default()
    }
}

fn parse_json_result(text: &str) -> Value {
    if let Ok(value) = serde_json::from_str(text) {
        return value;
    }
    if let Some(start) = text.find("```") {
        let rest = &text[start + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        if let Some(end) = rest.find("```")
            && let Ok(value) = serde_json::from_str(rest[..end].trim())
        {
            return value;
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && end >= start
        && let Ok(value) = serde_json::from_str(&text[start..=end])
    {
        return value;
    }
    Value::Null
}

fn resolve_in_workspace(root: &Path, path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(path);
    if candidate.is_absolute() || candidate.has_root() || path.split(['/', '\\']).any(|part| part == "..") {
        return Err(path_escapes_workspace(path));
    }
    let resolved_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let resolved = resolved_root.join(candidate);
    if resolved != resolved_root && !resolved.starts_with(&resolved_root) {
        return Err(path_escapes_workspace(path));
    }
    Ok(resolved)
}

fn path_escapes_workspace(path: &str) -> String {
    LocalizedText::with_params("v2Errors.pathEscapesWorkspace", i18n_params!["path" => path]).render()
}

fn glob_to_regex(pattern: &str) -> Result<Regex, regex::Error> {
    let pattern = pattern.replace('.', "\\.").replace("**", ".*").replace('*', "[^/]*").replace('?', "[^/]");
    Regex::new(&pattern)
}

fn glob_files(root: &Path, pattern: &str) -> Result<Vec<String>, String> {
    let regex = glob_to_regex(pattern).map_err(|error| error.to_string())?;
    let mut results = Vec::new();
    walk_dir(root, root, &regex, 0, &mut results);
    Ok(results)
}

fn walk_dir(root: &Path, directory: &Path, regex: &Regex, depth: usize, results: &mut Vec<String>) {
    if depth > MAX_GLOB_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            if GLOB_WALK_IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_dir(root, &path, regex, depth + 1, results);
        } else if results.len() < MAX_GLOB_RESULTS
            && let Ok(relative) = path.strip_prefix(root)
        {
            let relative = relative.to_string_lossy().replace('\\', "/");
            if regex.is_match(&relative) {
                results.push(relative);
            }
        }
    }
}

async fn run_shell(command: &str, cwd: &Path, timeout_ms: u64) -> WorkflowExecResult {
    #[cfg(windows)]
    let mut process = {
        let mut process = tokio::process::Command::new("cmd.exe");
        process.arg("/d").arg("/s").arg("/c").arg(format!("\"{command}\""));
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    process.current_dir(cwd);
    process.kill_on_drop(true);
    match tokio::time::timeout(Duration::from_millis(timeout_ms), process.output()).await {
        Ok(Ok(output)) => WorkflowExecResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(0),
        },
        Ok(Err(error)) => WorkflowExecResult {
            stdout: String::new(),
            stderr: error.to_string(),
            exit_code: 1,
        },
        Err(error) => WorkflowExecResult {
            stdout: String::new(),
            stderr: error.to_string(),
            exit_code: 1,
        },
    }
}

async fn fetch_result(url: &str, opts: WorkflowFetchOptions) -> WorkflowFetchResult {
    match perform_fetch(url, opts).await {
        Ok(result) => result,
        Err(error) => WorkflowFetchResult {
            ok: false,
            status: 0,
            body: error.to_string(),
        },
    }
}

async fn perform_fetch(url: &str, opts: WorkflowFetchOptions) -> Result<WorkflowFetchResult, WireAsyncError> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
    let method = reqwest::Method::from_bytes(opts.method.as_deref().unwrap_or("GET").as_bytes())?;
    let mut request = client.request(method, url);
    if let Some(headers) = opts.headers {
        for (name, value) in headers {
            request = request.header(name, value);
        }
    }
    if let Some(body) = opts.body {
        request = request.body(body);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.text().await?;
    Ok(WorkflowFetchResult {
        ok: status.is_success(),
        status: status.as_u16(),
        body,
    })
}

fn link_signals(parent: &AbortSignal, child: &Arc<AbortController>) -> AbortSignal {
    let _unlink = link_abort_signal(parent, child.clone());
    child.signal()
}

fn optional_json_to_js<'js>(ctx: &Ctx<'js>, value: Option<&Value>) -> rquickjs::Result<JsValue<'js>> {
    match value {
        Some(value) => json_to_js(ctx, value),
        None => Ok(JsValue::new_undefined(ctx.clone())),
    }
}

fn json_to_js<'js>(ctx: &Ctx<'js>, value: &Value) -> rquickjs::Result<JsValue<'js>> {
    Ok(match value {
        Value::Null => JsValue::new_null(ctx.clone()),
        Value::Bool(boolean) => JsValue::new_bool(ctx.clone(), *boolean),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                if let Ok(integer) = i32::try_from(integer) {
                    JsValue::new_int(ctx.clone(), integer)
                } else {
                    JsValue::new_float(ctx.clone(), integer as f64)
                }
            } else if let Some(float) = number.as_f64() {
                JsValue::new_float(ctx.clone(), float)
            } else {
                JsValue::new_null(ctx.clone())
            }
        }
        Value::String(text) => rquickjs::String::from_str(ctx.clone(), text)?.into_value(),
        Value::Array(items) => {
            let array = Array::new(ctx.clone())?;
            for (index, item) in items.iter().enumerate() {
                array.set(index, json_to_js(ctx, item)?)?;
            }
            array.into_value()
        }
        Value::Object(map) => {
            let object = Object::new(ctx.clone())?;
            for (key, item) in map {
                object.set(key.as_str(), json_to_js(ctx, item)?)?;
            }
            object.into_value()
        }
    })
}

fn js_to_optional_json(value: &JsValue<'_>) -> Option<Value> {
    if value.is_undefined() {
        None
    } else {
        Some(js_to_json(value))
    }
}

fn js_to_json(value: &JsValue<'_>) -> Value {
    if value.is_null() || value.is_undefined() {
        return Value::Null;
    }
    if let Some(boolean) = value.as_bool() {
        return Value::Bool(boolean);
    }
    if let Some(integer) = value.as_int() {
        return Value::from(integer);
    }
    if let Some(float) = value.as_float() {
        return serde_json::Number::from_f64(float).map(Value::Number).unwrap_or(Value::Null);
    }
    if let Some(text) = value.as_string() {
        return Value::String(text.to_string().unwrap_or_default());
    }
    if let Some(array) = value.as_array() {
        return Value::Array((0..array.len()).filter_map(|index| {
            array.get::<JsValue>(index).ok().map(|item| js_to_json(&item))
        }).collect());
    }
    if let Some(object) = value.as_object() {
        let mut map = serde_json::Map::new();
        for (key, item) in object.props::<String, JsValue>().flatten() {
            map.insert(key, js_to_json(&item));
        }
        return Value::Object(map);
    }
    Value::Null
}

fn json_string(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn render_js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    rquickjs::CaughtError::from_error(ctx, error).to_string()
}

fn workflow_error(message: impl Into<String>) -> WireAsyncError {
    Box::new(std::io::Error::other(message.into()))
}

pub fn workflow_now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_millis() as i64).unwrap_or(0)
}
