//! Turn-level context injection layer.
//!
//! Mirrors v2's AgentReminder mechanism
//! (`packages/agent-core-v2/src/features/reminder/`): before each LLM call
//! the turn loop asks registered providers for reminder texts and appends
//! them to the message history wrapped in `<system-reminder>…</system-reminder>`
//! (v2 `wrapSystemReminder`). Injection messages are identified by their
//! wrapper so the turn loop can keep them out of compaction trimming — the
//! Rust analog of v2's `origin.kind === 'injection'` classification.
//!
//! Built-in injections live here: the date-change reminder (v2 `dateChange`
//! variant) and the workspace-root AGENTS.md reminder (v2 `agents_md`
//! variant). The rest are contributed by their own modules — goal/plan mode
//! by [`goal_plan`], the permission-mode reminders by [`permission_mode`],
//! the post-interruption reminder by [`interruption_reminder`], and swarm
//! mode by [`swarm_mode`].

use std::path::{Path, PathBuf};

use crate::turn_loop::types::LLMMessage;

/// Wrapper prefix for injection texts, matching v2's `SYSTEM_REMINDER_PREFIX`.
pub const SYSTEM_REMINDER_PREFIX: &str = "<system-reminder>\n";
/// Wrapper suffix for injection texts, matching v2's `SYSTEM_REMINDER_SUFFIX`.
pub const SYSTEM_REMINDER_SUFFIX: &str = "\n</system-reminder>";

/// Goal/plan-mode injection providers.
pub mod goal_plan;

/// Permission-mode injection providers (v2 `PermissionModeInjection`).
pub mod permission_mode;

/// The interruption reminder (v2 `interruptionReminderService`): a one-shot
/// head-of-turn reminder after a user-cancelled turn.
pub mod interruption_reminder;

/// Swarm-mode reminders (v2 `SwarmInjection`, the `swarm_mode` variant).
pub mod swarm_mode;

/// The stale-todo reminder (v2 `todoListReminder`, the `todo_list_reminder`
/// variant).
pub mod notify_user_nudge;
pub mod todo_reminder;

/// The knowledge-base reminder and learner (v2 `KnowledgeInjection` /
/// `KnowledgeLearner`, the `knowledge` variant; main agent only).
pub mod knowledge;

/// The catalog-change reminders (v2 `agent/changeNotifier`): the
/// `skills_change`, `subagents_change` and `workspace_dirs_change` variants.
pub mod change_notifier;

/// The plugin session-start reminder (v2 `agent/plugin`): the
/// `plugin_session_start` variant.
pub mod plugin;

/// What the injection providers read their domain values from.
pub mod state;

pub use state::DomainValueSource;

/// Wrap an injection text in the `<system-reminder>` envelope. The content
/// is trimmed and placed between the prefix and suffix, exactly like v2's
/// `wrapSystemReminder`.
pub fn wrap_system_reminder(content: &str) -> String {
    format!(
        "{SYSTEM_REMINDER_PREFIX}{}{SYSTEM_REMINDER_SUFFIX}",
        content.trim()
    )
}

/// Whether a message content is a system-reminder injection (v2
/// `systemReminderContent` detection).
pub fn is_system_reminder(text: &str) -> bool {
    text.starts_with(SYSTEM_REMINDER_PREFIX) && text.ends_with(SYSTEM_REMINDER_SUFFIX)
}

/// Build a `user`-role message carrying an injection text. v2 appends
/// injections with role `user` and `origin.kind === 'injection'`; the Rust
/// engine marks them by their `<system-reminder>` content instead. The turn
/// loop uses [`injection_message_named`] so persisted history can distinguish
/// them from real user prompts.
pub fn injection_message(text: String) -> LLMMessage {
    LLMMessage {
        role: "user".into(),
        content: text,
        ..Default::default()
    }
}

/// Build an injection message carrying its variant in `origin`, the shape v2
/// uses (`{ kind: 'injection', variant }`). Undo anchor selection and the
/// replay renderer both key on it.
pub fn injection_message_named(text: String, variant: &str) -> LLMMessage {
    LLMMessage {
        role: "user".into(),
        content: text,
        origin: Some(serde_json::json!({
            "kind": "injection",
            "variant": variant,
        })),
        ..Default::default()
    }
}

/// Remove injection messages from `messages` in place and return them,
/// preserving order. The turn loop uses this to keep injections out of
/// compaction trimming: they are pulled out before compacting and
/// re-appended after.
pub fn split_injections(messages: &mut Vec<LLMMessage>) -> Vec<LLMMessage> {
    let mut injections = Vec::new();
    let mut kept = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        if is_system_reminder(&message.content) {
            injections.push(message);
        } else {
            kept.push(message);
        }
    }
    *messages = kept;
    injections
}

/// Context passed to injection providers for one build pass. Mirrors v2's
/// `ContextInjectionContext` (`isNewTurn` + the `injectedPositions` part):
/// whether this is the first step of the turn, and the names of injections
/// already appended this turn, in registration order.
/// What the previous step's tool calls did, handed to per-access providers.
///
/// v2 assembles the same three things from a `ToolDidExecuteContext`: the file
/// accesses become target directories (`:284-306`), an instruction file the
/// call read becomes `selfKnown` (`:297-302`), and a Bash call's own `cwd`
/// argument is contributed whether or not any operand resolves
/// (`:255-262, 274-276`).
#[derive(Debug, Clone, Default)]
pub struct StepAccess {
    /// Directories the calls touched: a file tool's parent, a tree search's
    /// own root.
    pub dirs: Vec<PathBuf>,
    /// Instruction files the calls read themselves.
    pub self_read: Vec<PathBuf>,
    /// Working directories Bash calls declared through their `cwd` argument.
    pub declared_cwds: Vec<String>,
}

impl StepAccess {
    /// True when nothing was recorded, so a provider can skip its probe.
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.self_read.is_empty() && self.declared_cwds.is_empty()
    }
}

pub struct InjectionContext<'a> {
    /// Whether this build pass runs at the first step of the turn (v2
    /// `isNewTurn`): turn-gated providers (goal) inject only on it.
    pub is_new_turn: bool,
    /// Whether a compaction splice re-armed this pass (v2
    /// `compactionRearmPending`). Turn-scoped providers re-derive their state
    /// from `history` when it is set: the splice can drop the disclosure they
    /// were tracking, and v2 reads the same state off the conversation.
    pub rearmed: bool,
    /// Names of injections already appended this turn, in registration order.
    pub injected: &'a [String],
    /// What the previous step's tool calls did. Empty when the caller does not
    /// track accesses, in which case per-access providers see no new ground.
    pub access: &'a StepAccess,
    /// The live message list at this build pass, including this turn's prior
    /// steps. Providers that recover state from the conversation (the todo
    /// reminder's turn counters) read it here; v2's equivalents read
    /// `memory.get()` at the same point in the step.
    pub history: &'a [LLMMessage],
}

/// A named injection provider: returns the raw reminder text for the current
/// step, or `None` when nothing should be injected. Providers may keep
/// per-turn state in their closure (e.g. the date-change tracker).
pub type InjectionProvider = Box<dyn FnMut(&InjectionContext) -> Option<String> + Send>;

struct InjectionEntry {
    name: String,
    provider: InjectionProvider,
}

/// Registry of injection providers, mirroring v2's
/// `AgentReminder.register(variant, provider)`.
pub struct InjectionRegistry {
    entries: Vec<InjectionEntry>,
    /// Names of injections already appended this turn, in registration order.
    injected: Vec<String>,
}

impl InjectionRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            injected: Vec::new(),
        }
    }

    /// Register a provider under a variant name (v2 `register(variant,
    /// provider)`). Providers run in registration order.
    pub fn register(&mut self, name: &str, provider: InjectionProvider) {
        self.entries.push(InjectionEntry {
            name: name.to_string(),
            provider,
        });
    }

    /// Create a registry with the built-in injections: the date-change
    /// reminder and the workspace-root AGENTS.md reminder. `workspace_root`
    /// is the session workspace the user opened — not the engine process's
    /// working directory, which can be anything. `date_baseline` seeds the
    /// date-change tracker with a previously disclosed date (scanned from
    /// history) so the baseline is not re-injected every turn;
    /// `agents_md_baseline` does the same for the AGENTS.md paths an earlier
    /// reminder already named.
    pub fn with_defaults(
        workspace_root: Option<PathBuf>,
        date_baseline: Option<String>,
        agents_md_baseline: Vec<String>,
    ) -> Self {
        let mut registry = Self::new();
        registry.register("date_change", Box::new(date_change_provider(date_baseline)));
        registry.register(
            "agents_md",
            Box::new(agents_md_provider(workspace_root, agents_md_baseline)),
        );
        registry
    }

    /// Names of registered providers, in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    /// Forget the names recorded by earlier passes of this turn. The turn loop
    /// calls this when a compaction splice re-arms the reminders: v2 re-runs
    /// the providers with `isNewTurn = true`, and its injected positions come
    /// from the conversation, so the spliced history no longer shows an
    /// earlier injection. Providers that consult `ctx.injected` (the
    /// knowledge base) must therefore see an empty list again.
    pub fn clear_injected(&mut self) {
        self.injected.clear();
    }

    /// Run every provider and return the wrapped injection texts for this
    /// step, in registration order. `is_new_turn` gates turn-scoped providers
    /// (v2 `isNewTurn`): they inject at the turn's first step and again after
    /// a mid-turn compaction rearm. Providers that return `None` or blank text
    /// contribute nothing; successful injections are recorded in the context
    /// handed to later providers.
    pub fn build_injections(&mut self, is_new_turn: bool) -> Vec<String> {
        self.build_injections_with_accesses(is_new_turn, &StepAccess::default())
    }

    /// Run every provider with what the previous step's tool calls did, so
    /// per-access providers (the AGENTS.md reminder) can discover instruction
    /// files in the subtrees those calls touched.
    pub fn build_injections_with_accesses(
        &mut self,
        is_new_turn: bool,
        access: &StepAccess,
    ) -> Vec<String> {
        self.build_injections_with_history(is_new_turn, access, &[])
    }

    /// Run every provider with the live message list, so history-derived
    /// providers see the same conversation v2's `memory.get()` returns at this
    /// point in the step. The turn loop passes `messages` here; callers that
    /// have no history (tests, one-shot probes) can use
    /// [`Self::build_injections_with_accesses`] instead.
    pub fn build_injections_with_history(
        &mut self,
        is_new_turn: bool,
        access: &StepAccess,
        history: &[LLMMessage],
    ) -> Vec<String> {
        self.build_injections_with_history_named(is_new_turn, access, history)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    /// [`Self::build_injections_with_history`] plus each injection's variant
    /// name, so the caller can stamp `origin.kind = "injection"` on the message.
    pub fn build_injections_with_history_named(
        &mut self,
        is_new_turn: bool,
        access: &StepAccess,
        history: &[LLMMessage],
    ) -> Vec<(String, String)> {
        self.build_injections_rearmed(is_new_turn, false, access, history)
    }

    /// [`Self::build_injections_with_history_named`] with the compaction rearm
    /// flag: the turn loop sets it for the single pass that follows a splice,
    /// so turn-scoped providers re-derive their state from the new history
    /// instead of trusting a baseline the splice may have invalidated.
    pub fn build_injections_rearmed(
        &mut self,
        is_new_turn: bool,
        rearmed: bool,
        access: &StepAccess,
        history: &[LLMMessage],
    ) -> Vec<(String, String)> {
        let mut texts = Vec::new();
        for entry in &mut self.entries {
            let ctx = InjectionContext {
                is_new_turn,
                rearmed,
                injected: &self.injected,
                access,
                history,
            };
            if let Some(content) = (entry.provider)(&ctx)
                && !content.trim().is_empty()
            {
                self.injected.push(entry.name.clone());
                texts.push((entry.name.clone(), wrap_system_reminder(&content)));
            }
        }
        texts
    }
}

impl Default for InjectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Built-in injections ─────────────────────────────────────────────────────

/// State machine for the date-change reminder (v2 `dateChange` variant):
/// records the last disclosed date and produces the baseline/change texts.
/// Pure — the caller supplies the current date, so tests can drive day
/// boundaries deterministically.
pub struct DateChangeTracker {
    last_date: Option<String>,
}

impl DateChangeTracker {
    /// Create a tracker with no disclosed date yet.
    pub fn new() -> Self {
        Self { last_date: None }
    }

    /// Create a tracker seeded with a previously disclosed date (v2's
    /// history-scanned `lastDisclosure`): the baseline survives across turns,
    /// so a matching seed suppresses the baseline re-injection until the date
    /// actually changes.
    pub fn with_last_date(last_date: Option<String>) -> Self {
        Self { last_date }
    }

    /// Feed the current date and timezone; returns the reminder text to inject
    /// for this step, or `None` when the date is unchanged since the last
    /// disclosure. The first call injects the baseline date (v2's seed
    /// disclosure); later calls inject only when the date changed. Both texts
    /// name the local timezone, as v2's do (`dateChangeService.ts:110,127`).
    pub fn step(&mut self, today: &str, time_zone: &str) -> Option<String> {
        match &self.last_date {
            None => {
                self.last_date = Some(today.to_string());
                Some(format!(
                    "Today's date is {today}, in the local client machine's timezone \
                     ({time_zone}). The current date is restated in a reminder whenever it \
                     changes; rely on the latest such reminder for the current date. DO NOT \
                     mention this to the user explicitly."
                ))
            }
            Some(previous) if previous != today => {
                self.last_date = Some(today.to_string());
                Some(format!(
                    "The date has changed. Today's date is now {today}, in the local client \
                     machine's timezone ({time_zone}). Rely on this reminder over any earlier \
                     date statement for the current date. DO NOT mention this to the user \
                     explicitly."
                ))
            }
            Some(_) => None,
        }
    }
}

impl Default for DateChangeTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Provider for the date-change reminder: injects the current date on the
/// first pass of a turn unless a prior disclosure was already scanned from
/// history, and re-injects whenever the date changes mid-turn.
///
/// A mid-turn compaction splice re-runs the providers with `is_new_turn` set
/// (v2's `compactionRearmPending` rearm), and v2's provider derives the last
/// disclosure from history — so a disclosure the splice dropped is injected
/// again. Re-deriving here at every turn head, rearm included, is that same
/// rule; the constructor baseline only matters for callers with no history.
fn date_change_provider(
    baseline: Option<String>,
) -> impl FnMut(&InjectionContext) -> Option<String> {
    let mut tracker = DateChangeTracker::with_last_date(baseline);
    let time_zone = local_timezone_label();
    move |ctx: &InjectionContext| {
        if ctx.rearmed {
            tracker = DateChangeTracker::with_last_date(scan_date_baseline(ctx.history));
        }
        tracker.step(&today_local(), &time_zone)
    }
}

/// Current local date as `YYYY-MM-DD` (v2 discloses the host-clock local
/// date via `Intl.DateTimeFormat`; chrono's `Local` resolves the system
/// timezone).
fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// The local timezone label the reminder names (v2 `IHostClock.timeZone()`):
/// the IANA name when the environment supplies one (`TZ`), otherwise the
/// numeric UTC offset chrono resolves for the host clock.
fn local_timezone_label() -> String {
    if let Ok(zone) = std::env::var("TZ") {
        let zone = zone.trim();
        if !zone.is_empty() {
            return zone.to_string();
        }
    }
    format!("UTC{}", chrono::Local::now().offset())
}

/// Scan prior conversation messages for the last date-disclosure reminder and
/// extract the disclosed date (v2's history-scanned `lastDisclosure`): the
/// baseline survives across turns, so the date is only re-injected when it
/// actually changes instead of at the first step of every turn.
pub fn scan_date_baseline(messages: &[LLMMessage]) -> Option<String> {
    const MARKER: &str = "Today's date is";
    for message in messages.iter().rev() {
        let content = message.content.as_str();
        // Only an injected reminder is a disclosure; a user quoting the
        // sentence must not move the baseline (v2 reads the value off the
        // injection's origin, never off message text).
        if !crate::injection::is_system_reminder(content) {
            continue;
        }
        let Some(at) = content.rfind(MARKER) else {
            continue;
        };
        // The change text reads "Today's date is now <date>"; the baseline
        // reads "Today's date is <date>". Skip the optional " now ".
        let mut after = &content[at + MARKER.len()..];
        after = after.strip_prefix(" now ").unwrap_or(after);
        after = after.strip_prefix(' ').unwrap_or(after);
        let date: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        if date.len() == 10 {
            return Some(date);
        }
    }
    None
}

/// The instruction files directly under `root` that the runtime reminder
/// suggests (v2 `findAgentsMdInDir`): the `.kimi-code/AGENTS.md` variant plus
/// the first non-empty of `AGENTS.md` / `agents.md`.
///
/// An empty or whitespace-only file is not an instruction file (v2
/// `isNonEmptyFile`) — the same rule the init-time loader applies — so a
/// placeholder file never earns a reminder.
pub fn find_agents_md(root: &Path) -> Vec<PathBuf> {
    // v2 probes the whole chain from the work-tree root down to the anchor
    // directory (`probeDir` → `dirsRootToLeaf`), so a session running in a
    // subdirectory is reminded about the repository root's files too.
    crate::prompt::agents_md::instruction_dirs(root)
        .iter()
        .flat_map(|dir| find_agents_md_in_dir(dir))
        .collect()
}

/// The AGENTS.md files that live directly in `dir` (v2 `agentsMdCandidatePaths`
/// order: `.kimi-code/AGENTS.md` first, then the plain names).
pub(crate) fn find_agents_md_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let dot_kimi = dir.join(".kimi-code").join("AGENTS.md");
    if is_non_empty_file(&dot_kimi) {
        found.push(dot_kimi);
    }
    for name in ["AGENTS.md", "agents.md"] {
        let candidate = dir.join(name);
        if is_non_empty_file(&candidate) {
            found.push(candidate);
            break;
        }
    }
    found
}

/// v2 `isNonEmptyFile`: readable and not whitespace-only.
fn is_non_empty_file(path: &Path) -> bool {
    std::fs::read(path)
        .map(|bytes| !String::from_utf8_lossy(&bytes).trim().is_empty())
        .unwrap_or(false)
}

/// Provider for the AGENTS.md reminder (v2 `agents_md` variant), backed by
/// [`AgentsMdReminder`].
///
/// `root` is the workspace root, the top of every probed chain; `None` when
/// the process working directory is unavailable. `disclosed` carries the paths
/// an earlier reminder already named, scanned out of history because the fork's
/// messages carry no `origin.disclosure` (v2 keeps the set on the last
/// injection) — the same substitution [`scan_agents_md_baseline`] makes for the
/// other variants.
fn agents_md_provider(
    root: Option<PathBuf>,
    disclosed: Vec<String>,
) -> impl FnMut(&InjectionContext) -> Option<String> {
    // The prompt-derived paths stay known across a rearm; only the
    // history-derived half is re-read, because a compaction splice can drop
    // the reminder that disclosed a path while it cannot change what the
    // system prompt already listed.
    let seed = disclosed;
    let mut reminder = AgentsMdReminder::new(root.clone(), seed.clone());
    move |ctx: &InjectionContext| {
        if ctx.rearmed {
            let mut known = seed.clone();
            for path in scan_agents_md_baseline(ctx.history) {
                if !known.contains(&path) {
                    known.push(path);
                }
            }
            reminder = AgentsMdReminder::new(root.clone(), known);
        }
        reminder.observe(ctx.access);
        let fresh = reminder.take_pending();
        if fresh.is_empty() {
            return None;
        }
        Some(agents_md_reminder_text(&fresh))
    }
}

/// The reminder body v2 renders for a non-empty set of discovered paths
/// (`agentsMdReminderService.ts:353-359`), verbatim.
fn agents_md_reminder_text(paths: &[String]) -> String {
    let listed = paths
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The following AGENTS.md file(s) apply to paths accessed by your recent tool call, \
         but were not included in your system prompt:\n{listed}\n\
         Read them before making changes in those directories."
    )
}

/// The instruction-file basenames, as an [`AgentsMdReminder`] lookup set.
fn is_agents_md_name(name: &str) -> bool {
    name == "AGENTS.md" || name == "agents.md"
}

/// State machine for the AGENTS.md reminder (v2 `AgentsMdReminderService`).
///
/// v2 hooks `onDidExecuteTool`, walks from the project root down to every
/// directory the call touched, and queues any AGENTS.md it has not already
/// disclosed. The fork's provider runs at the same observation point — before
/// each LLM call, which is after the previous step's tool calls — so the only
/// input v2 gets for free and this does not is *which* directories were
/// touched; that arrives as [`InjectionContext::accessed_dirs`].
///
/// v2 resolves the chain against `findProjectRoot`. This walks from the
/// workspace root instead: that is the boundary the rest of the fork already
/// enforces, and it avoids introducing a second notion of "project" that could
/// disagree with the sandbox.
pub struct AgentsMdReminder {
    /// AGENTS.md paths already in the instructions or already suggested.
    known: std::collections::HashSet<String>,
    /// Discovered by a probe but not yet suggested.
    queue: Vec<String>,
    /// Paths the model itself just read, so naming them back is noise (v2
    /// `selfKnown`, which suppresses the reminder for the rest of the step).
    read_recently: std::collections::HashSet<String>,
    /// Workspace root, the top of every probed chain.
    root: Option<PathBuf>,
}

impl AgentsMdReminder {
    /// Seed a reminder. `disclosed` are the paths history already named; they
    /// start out known so a resumed session does not re-suggest them.
    pub fn new(root: Option<PathBuf>, disclosed: Vec<String>) -> Self {
        Self {
            known: disclosed.into_iter().collect(),
            queue: Vec::new(),
            read_recently: std::collections::HashSet::new(),
            root,
        }
    }

    /// Record one step's tool activity and probe what it reached.
    pub fn observe(&mut self, access: &StepAccess) {
        // v2 `trackChanges` (`agentsMdReminderService.ts:212-240`): a watched
        // file that disappeared drops out of `known`, so recreating it earns a
        // fresh reminder; a modification leaves it known. Drop only what is
        // provably gone: an unreadable directory (EACCES) is not evidence that
        // the file no longer exists, and removing it would earn a duplicate
        // reminder the moment the permission comes back.
        self.known.retain(|path| {
            !matches!(
                std::fs::metadata(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            )
        });
        for path in &access.self_read {
            self.read_recently.insert(normalize_agents_md(path));
        }
        for dir in &access.dirs {
            self.probe_chain(dir);
        }
        self.observe_declared_cwds(&access.declared_cwds);
    }

    /// A Bash call's own `cwd` argument, which v2 contributes whether or not
    /// any operand resolves (`agentsMdReminderService.ts:255-262, 274-276`).
    ///
    /// v2's *other* half — the operand directories from `extractBashTargetDirs`
    /// — needs a bash syntax tree, and this engine has no parser: see
    /// `native/permission_engine/dangerous_command.rs`, which takes the same
    /// absence as an accepted, conservative degradation. Missing a `cwd` here
    /// costs the reminder one directory, which is the safe direction to be
    /// wrong in; guessing a wrong one would point the model at a file it has no
    /// reason to read.
    fn observe_declared_cwds(&mut self, cwds: &[String]) {
        for cwd in cwds {
            if cwd.is_empty() {
                continue;
            }
            let candidate = Path::new(cwd);
            // Only a relative `cwd` needs the workspace root; an absolute one
            // is already resolved, and dropping it because the root is
            // unavailable would lose a directory this call really touched.
            let resolved = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else if let Some(root) = &self.root {
                root.join(candidate)
            } else {
                continue;
            };
            self.probe_chain(&resolved);
        }
    }

    /// Probe the chain from the work-tree root down to `dir`, queueing every
    /// AGENTS.md found on the way.
    ///
    /// v2 resolves the anchor's own project root (`findProjectRoot`) rather
    /// than the session's: a tool that reads a path outside the workspace still
    /// gets that path's AGENTS.md. A directory with no work tree is its own
    /// root, so the chain is just itself.
    fn probe_chain(&mut self, dir: &Path) {
        let resolved = if dir.is_absolute() {
            dir.to_path_buf()
        } else if let Some(root) = &self.root {
            root.join(dir)
        } else {
            dir.to_path_buf()
        };
        // Inside the workspace the chain starts at the workspace root, so a
        // session rooted at a subdirectory still sees the root's file; outside
        // it starts at the anchor's own work-tree root.
        let chain = match self.root.as_ref().filter(|root| resolved.starts_with(root)) {
            Some(root) => crate::prompt::agents_md::dirs_root_to_leaf(root, &resolved),
            None => crate::prompt::agents_md::instruction_dirs(&resolved),
        };
        for dir in chain {
            for found in find_agents_md_in_dir(&dir) {
                self.queue_path(&found);
            }
        }
    }

    fn queue_path(&mut self, found: &Path) {
        let key = normalize_agents_md(found);
        if !self.known.contains(&key) && !self.queue.contains(&key) {
            self.queue.push(key);
        }
    }

    /// Drain the queue into the paths worth suggesting now: anything not
    /// already known and not one the model just read. The returned paths become
    /// known, so each is suggested at most once.
    pub fn take_pending(&mut self) -> Vec<String> {
        let queued = std::mem::take(&mut self.queue);
        let fresh: Vec<String> = queued
            .into_iter()
            .filter(|path| !self.known.contains(path) && !self.read_recently.contains(path))
            .collect();
        for path in &fresh {
            self.known.insert(path.clone());
        }
        // v2 clears `readRecently` per injection pass (`:153-154`).
        self.read_recently.clear();
        fresh
    }

    /// Paths already in the instructions or already suggested. Exposed for
    /// tests and for the ledger; not part of the injection contract.
    pub fn known_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.known.iter().cloned().collect();
        paths.sort();
        paths
    }
}

/// A comparable form of an AGENTS.md path: forward slashes, no trailing
/// separator. v2 normalizes the same way before putting a path in its `known`
/// set (`normalize` in `agentsMdReminderService.ts`), so a path discovered by a
/// probe and the same path named in history compare equal.
fn normalize_agents_md(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    text.trim_end_matches('/').to_string()
}

/// Whether a tool that read `path` read an instruction file, i.e. whether it
/// belongs in the reminder's `selfKnown` set.
pub fn is_agents_md_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_agents_md_name)
}

/// The AGENTS.md paths an earlier reminder already disclosed. v2 keeps the set
/// on the injection message's `origin.disclosure`; the fork's messages carry
/// no origin, so the reminder text is scanned instead — the same shape as
/// [`scan_date_baseline`].
pub fn scan_agents_md_baseline(messages: &[LLMMessage]) -> Vec<String> {
    const MARKER: &str =
        "The following AGENTS.md file(s) apply to paths accessed by your recent tool call";
    let mut disclosed: Vec<String> = Vec::new();
    for message in messages {
        let content = message.content.as_str();
        // Only an injected reminder discloses paths; a user or tool message
        // quoting the listing must not seed the known set (v2 reads the set
        // off the injection's origin, not off arbitrary message text).
        if !crate::injection::is_system_reminder(content) {
            continue;
        }
        if !content.contains(MARKER) {
            continue;
        }
        for line in content.lines() {
            let Some(path) = line.strip_prefix("- ") else {
                continue;
            };
            let path = path.trim();
            if !path.is_empty() && !disclosed.iter().any(|seen| seen == path) {
                disclosed.push(path.to_string());
            }
        }
    }
    disclosed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_system_reminder_format() {
        // Basic whitespace trimming
        let wrapped = wrap_system_reminder("  hello world  ");
        assert_eq!(
            wrapped,
            "<system-reminder>\nhello world\n</system-reminder>"
        );

        // Multiline content preservation
        let multiline = wrap_system_reminder(" \n line 1 \n  line 2\n ");
        assert_eq!(
            multiline,
            "<system-reminder>\nline 1 \n  line 2\n</system-reminder>"
        );

        // Empty and whitespace-only strings
        assert_eq!(
            wrap_system_reminder(""),
            "<system-reminder>\n\n</system-reminder>"
        );
        assert_eq!(
            wrap_system_reminder("   \t\r\n   "),
            "<system-reminder>\n\n</system-reminder>"
        );

        // Structural invariant with prefix and suffix
        assert!(wrapped.starts_with(SYSTEM_REMINDER_PREFIX));
        assert!(wrapped.ends_with(SYSTEM_REMINDER_SUFFIX));
    }

    #[test]
    fn test_is_system_reminder_detection() {
        // Valid reminders
        assert!(is_system_reminder(
            "<system-reminder>\nhello\n</system-reminder>"
        ));
        assert!(is_system_reminder(
            "<system-reminder>\nline1\nline2\n</system-reminder>"
        ));
        assert!(is_system_reminder(
            "<system-reminder>\n\n</system-reminder>"
        ));

        // Invalid: missing tags or malformed delimiters
        assert!(!is_system_reminder("hello"));
        assert!(!is_system_reminder(""));
        assert!(!is_system_reminder("<system-reminder>\nhello"));
        assert!(!is_system_reminder("hello\n</system-reminder>"));
        assert!(!is_system_reminder(
            "<system-reminder>hello\n</system-reminder>"
        ));
        assert!(!is_system_reminder(
            "<system-reminder>\nhello</system-reminder>"
        ));
        assert!(!is_system_reminder(
            " <system-reminder>\nhello\n</system-reminder>"
        ));
        assert!(!is_system_reminder(
            "<system-reminder>\nhello\n</system-reminder> "
        ));
    }

    #[test]
    fn test_injection_message_shape() {
        let content = wrap_system_reminder("system instruction payload");
        let message = injection_message(content.clone());
        assert_eq!(message.role, "user");
        assert_eq!(message.content, content);
        assert!(is_system_reminder(&message.content));
        assert!(message.blocks.is_empty());
        assert!(message.tool_calls.is_empty());
        assert!(message.tool_call_id.is_none());
    }

    /// Persisted history must be able to tell an injection from a real user
    /// prompt; undo anchor selection keys on `origin.kind`.
    #[test]
    fn test_named_injection_message_carries_origin() {
        let message = injection_message_named(wrap_system_reminder("x"), "todo_list_reminder");
        let origin = message.origin.expect("origin");
        assert_eq!(origin["kind"], "injection");
        assert_eq!(origin["variant"], "todo_list_reminder");
    }

    #[test]
    fn test_named_build_returns_the_variant() {
        let mut registry = InjectionRegistry::new();
        registry.register("date_change", Box::new(|_| Some("hello".into())));
        let named = registry.build_injections_with_history_named(true, &StepAccess::default(), &[]);
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].0, "date_change");
        assert!(named[0].1.contains("hello"));
    }

    #[test]
    fn test_split_injections_separates_and_preserves_order() {
        let mut messages = vec![
            LLMMessage {
                role: "user".into(),
                content: "plain user message".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: wrap_system_reminder("reminder 1"),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: wrap_system_reminder("reminder 2"),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "assistant reply".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: wrap_system_reminder("reminder 3"),
                ..Default::default()
            },
        ];

        let injections = split_injections(&mut messages);

        // Injections separated with exact count and content in original order
        assert_eq!(injections.len(), 3);
        assert_eq!(injections[0].role, "user");
        assert_eq!(injections[0].content, wrap_system_reminder("reminder 1"));
        assert_eq!(injections[1].role, "user");
        assert_eq!(injections[1].content, wrap_system_reminder("reminder 2"));
        assert_eq!(injections[2].role, "user");
        assert_eq!(injections[2].content, wrap_system_reminder("reminder 3"));

        // Remaining non-injection messages preserved exactly in order
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "plain user message");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "assistant reply");

        // Edge case: all injections
        let mut all_inj = vec![
            LLMMessage {
                role: "user".into(),
                content: wrap_system_reminder("a"),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: wrap_system_reminder("b"),
                ..Default::default()
            },
        ];
        let split_all = split_injections(&mut all_inj);
        assert!(all_inj.is_empty());
        assert_eq!(split_all.len(), 2);
        assert_eq!(split_all[0].content, wrap_system_reminder("a"));
        assert_eq!(split_all[1].content, wrap_system_reminder("b"));

        // Edge case: no injections
        let mut no_inj = vec![LLMMessage {
            role: "user".into(),
            content: "regular".into(),
            ..Default::default()
        }];
        let split_none = split_injections(&mut no_inj);
        assert!(split_none.is_empty());
        assert_eq!(no_inj.len(), 1);
        assert_eq!(no_inj[0].content, "regular");

        // Edge case: empty input
        let mut empty: Vec<LLMMessage> = Vec::new();
        let split_empty = split_injections(&mut empty);
        assert!(split_empty.is_empty());
        assert!(empty.is_empty());
    }

    #[test]
    fn test_registry_register_and_build() {
        let mut registry = InjectionRegistry::new();
        registry.register("a", Box::new(|_| Some("first".into())));
        registry.register("b", Box::new(|_| None));
        registry.register("c", Box::new(|_| Some("   ".into())));
        registry.register("d", Box::new(|_| Some("fourth\nline2".into())));

        assert_eq!(registry.names(), vec!["a", "b", "c", "d"]);

        let texts = registry.build_injections(true);
        assert_eq!(
            texts.len(),
            2,
            "None and whitespace-only providers contribute nothing"
        );
        assert_eq!(texts[0], wrap_system_reminder("first"));
        assert_eq!(texts[1], wrap_system_reminder("fourth\nline2"));
    }

    #[test]
    fn test_registry_context_exposes_injected_names() {
        let mut registry = InjectionRegistry::new();
        registry.register(
            "first",
            Box::new(|ctx| {
                assert!(
                    ctx.injected.is_empty(),
                    "first provider sees empty injected slice"
                );
                Some("one".into())
            }),
        );
        registry.register(
            "skipped",
            Box::new(|ctx| {
                assert_eq!(ctx.injected, &["first"]);
                None
            }),
        );
        registry.register(
            "blank",
            Box::new(|ctx| {
                assert_eq!(ctx.injected, &["first"]);
                Some("   ".into())
            }),
        );
        registry.register(
            "second",
            Box::new(|ctx| {
                assert_eq!(ctx.injected, &["first"]);
                Some("two".into())
            }),
        );
        registry.register(
            "third",
            Box::new(|ctx| {
                assert_eq!(ctx.injected, &["first", "second"]);
                Some("three".into())
            }),
        );
        let texts = registry.build_injections(true);
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[0], wrap_system_reminder("one"));
        assert_eq!(texts[1], wrap_system_reminder("two"));
        assert_eq!(texts[2], wrap_system_reminder("three"));
    }

    #[test]
    fn test_with_defaults_registers_builtins_and_builds() {
        let mut registry = InjectionRegistry::with_defaults(None, None, Vec::new());
        assert_eq!(registry.names(), vec!["date_change", "agents_md"]);

        let texts = registry.build_injections(true);
        assert!(
            !texts.is_empty(),
            "with_defaults must produce at least the date_change reminder"
        );
        assert!(is_system_reminder(&texts[0]));
        assert!(texts[0].starts_with("<system-reminder>\nToday's date is "));

        let empty_reg = InjectionRegistry::default();
        assert!(empty_reg.names().is_empty());
    }

    /// v2 `compactionRearmPending`: a splice mid-turn drops the injection
    /// messages the compacted prefix held, so the providers run again with
    /// `isNewTurn = true` and re-derive their state from the new history. A
    /// reminder the splice dropped must therefore come back exactly once.
    #[test]
    fn test_rearm_reinjects_a_reminder_the_compaction_splice_dropped() {
        let disclosure = wrap_system_reminder(&format!(
            "Today's date is {}, in the local client machine's timezone (Asia/Shanghai).",
            today_local()
        ));
        let history = vec![LLMMessage {
            role: "user".into(),
            content: disclosure,
            ..Default::default()
        }];
        let mut registry =
            InjectionRegistry::with_defaults(None, scan_date_baseline(&history), Vec::new());

        // Turn head: history already discloses today, so the baseline is
        // suppressed.
        assert!(
            registry
                .build_injections_rearmed(true, false, &StepAccess::default(), &history)
                .is_empty(),
            "the disclosure in history suppresses the turn-head baseline"
        );

        // The compaction spliced the disclosure away.
        let spliced = vec![LLMMessage {
            role: "user".into(),
            content: "<summary>earlier conversation</summary>".into(),
            ..Default::default()
        }];
        let rearmed =
            registry.build_injections_rearmed(true, true, &StepAccess::default(), &spliced);
        assert_eq!(
            rearmed.len(),
            1,
            "the rearm re-injects a reminder the splice dropped: {rearmed:?}"
        );
        assert!(rearmed[0].1.contains("Today's date is"));

        // Once the re-injected reminder is in the new history, further passes
        // stay silent.
        let mut after = spliced;
        after.push(injection_message(rearmed[0].1.clone()));
        assert!(
            registry
                .build_injections_rearmed(true, true, &StepAccess::default(), &after)
                .is_empty(),
            "a reminder that is back in history is not injected twice"
        );
    }

    #[test]
    fn test_date_change_tracker_baseline() {
        let mut tracker = DateChangeTracker::new();
        let text = tracker
            .step("2026-09-02", "Asia/Shanghai")
            .expect("first pass injects the baseline");
        assert_eq!(
            text,
            "Today's date is 2026-09-02, in the local client machine's timezone \
             (Asia/Shanghai). The current date is restated in a reminder whenever it changes; \
             rely on the latest such reminder for the current date. DO NOT mention this to the \
             user explicitly."
        );
    }

    #[test]
    fn test_date_change_tracker_unchanged_is_silent() {
        let mut tracker = DateChangeTracker::default();
        let first = tracker.step("2026-09-02", "Asia/Shanghai");
        assert!(first.is_some());
        assert_eq!(
            tracker.step("2026-09-02", "Asia/Shanghai"),
            None,
            "same date must not re-inject"
        );
        assert_eq!(
            tracker.step("2026-09-02", "Asia/Shanghai"),
            None,
            "repeated calls with same date must remain silent"
        );
    }

    #[test]
    fn test_date_change_tracker_crosses_day() {
        let mut tracker = DateChangeTracker::new();
        tracker.step("2026-09-02", "Asia/Shanghai");

        let text = tracker
            .step("2026-09-03", "Asia/Shanghai")
            .expect("day change injects");
        assert_eq!(
            text,
            "The date has changed. Today's date is now 2026-09-03, in the local client \
             machine's timezone (Asia/Shanghai). Rely on this reminder over any earlier date \
             statement for the current date. DO NOT mention this to the user explicitly."
        );

        assert_eq!(
            tracker.step("2026-09-03", "Asia/Shanghai"),
            None,
            "no repeat after the change"
        );

        let text2 = tracker
            .step("2026-09-04", "Asia/Shanghai")
            .expect("next day change injects");
        assert_eq!(
            text2,
            "The date has changed. Today's date is now 2026-09-04, in the local client \
             machine's timezone (Asia/Shanghai). Rely on this reminder over any earlier date \
             statement for the current date. DO NOT mention this to the user explicitly."
        );
    }

    #[test]
    fn test_today_local_formats() {
        let today = today_local();
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3, "today_local must format as YYYY-MM-DD");
        let year: i64 = parts[0].parse().expect("valid year");
        let month: u32 = parts[1].parse().expect("valid month");
        let day: u32 = parts[2].parse().expect("valid day");

        assert!(year >= 2024, "year must be realistic modern date");
        assert!((1..=12).contains(&month), "month must be in 1..=12");
        assert!((1..=31).contains(&day), "day must be in 1..=31");
    }

    #[test]
    fn test_scan_date_baseline_finds_last_disclosure() {
        let baseline = "Today's date is 2026-09-08. The current date is restated in a \
                        reminder whenever it changes; rely on the latest such reminder \
                        for the current date. DO NOT mention this to the user explicitly.";
        let change = "The date has changed. Today's date is now 2026-09-09. Rely on this \
                      reminder over any earlier date statement for the current date. DO \
                      NOT mention this to the user explicitly.";
        let msg = |text: &str| LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(text),
            ..Default::default()
        };
        assert_eq!(scan_date_baseline(&[]), None);
        assert_eq!(
            scan_date_baseline(&[msg(baseline)]),
            Some("2026-09-08".into())
        );
        // The latest disclosure wins.
        assert_eq!(
            scan_date_baseline(&[msg(baseline), msg("unrelated"), msg(change)]),
            Some("2026-09-09".into())
        );
        // Non-date text with a coincidental prefix must not parse.
        assert_eq!(
            scan_date_baseline(&[msg("Today's date is somewhere")]),
            None
        );
        // A plain user message that happens to contain the sentence is not a
        // disclosure: v2 reads the value off the injection's origin, so text
        // alone must not move the baseline.
        assert_eq!(
            scan_date_baseline(&[LLMMessage {
                role: "user".into(),
                content: change.to_string(),
                ..Default::default()
            }]),
            None
        );
    }

    /// v2 `probeDir` walks `dirsRootToLeaf`: a session running in a
    /// subdirectory is reminded about the work-tree root's instructions too,
    /// root first.
    #[test]
    fn test_find_agents_md_walks_from_the_work_tree_root() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("packages").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "# Root").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "# Nested").unwrap();

        let found = find_agents_md(&nested);
        assert_eq!(
            found,
            vec![repo.path().join("AGENTS.md"), nested.join("AGENTS.md")],
            "root first, then the leaf: {found:?}"
        );
    }

    #[test]
    fn test_find_agents_md_cases() {
        let dir = tempfile::tempdir().unwrap();

        assert!(find_agents_md(dir.path()).is_empty());

        let uppercase = dir.path().join("AGENTS.md");
        std::fs::write(&uppercase, "# Instructions").unwrap();
        let found = find_agents_md(dir.path());
        assert_eq!(found, vec![uppercase]);
        assert_eq!(found[0].file_name().unwrap(), "AGENTS.md");

        let dir2 = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir2.path().join("AGENTS.md")).unwrap();
        assert!(
            find_agents_md(dir2.path()).is_empty(),
            "directory named AGENTS.md must not match"
        );

        let dir3 = tempfile::tempdir().unwrap();
        let lowercase = dir3.path().join("agents.md");
        std::fs::write(&lowercase, "# Instructions").unwrap();
        let found_lower = find_agents_md(dir3.path());
        assert_eq!(found_lower.len(), 1);
        assert!(found_lower[0].is_file());
        assert_eq!(
            std::fs::read_to_string(&found_lower[0]).unwrap(),
            "# Instructions"
        );
        assert_eq!(found_lower[0].parent().unwrap(), dir3.path());

        // Both plain names present: the uppercase one wins, and only one is
        // listed (v2's `break` inside `findAgentsMdInDir`).
        let dir4 = tempfile::tempdir().unwrap();
        std::fs::write(dir4.path().join("AGENTS.md"), "# Upper").unwrap();
        std::fs::write(dir4.path().join("agents.md"), "# Lower").unwrap();
        let found_prec = find_agents_md(dir4.path());
        assert_eq!(found_prec.len(), 1);
        assert_eq!(found_prec[0].file_name().unwrap(), "AGENTS.md");
    }

    /// v2 `findAgentsMdInDir`: the `.kimi-code/AGENTS.md` variant is listed
    /// alongside the plain file, and a whitespace-only file is not an
    /// instruction file (v2 `isNonEmptyFile`, the same rule the init-time
    /// loader applies).
    #[test]
    fn test_find_agents_md_discovers_dot_kimi_and_ignores_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".kimi-code")).unwrap();
        std::fs::write(dir.path().join(".kimi-code/AGENTS.md"), "# House rules").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Instructions").unwrap();
        let found = find_agents_md(dir.path());
        assert_eq!(found.len(), 2, "both variants: {found:?}");
        assert_eq!(found[0].file_name().unwrap(), "AGENTS.md");
        assert_eq!(
            found[0].parent().unwrap().file_name().unwrap(),
            ".kimi-code"
        );
        assert_eq!(found[1], dir.path().join("AGENTS.md"));

        let empty = tempfile::tempdir().unwrap();
        std::fs::write(empty.path().join("AGENTS.md"), "  \n\t\n").unwrap();
        assert!(
            find_agents_md(empty.path()).is_empty(),
            "a whitespace-only AGENTS.md is not an instruction file"
        );
        std::fs::create_dir(empty.path().join(".kimi-code")).unwrap();
        std::fs::write(empty.path().join(".kimi-code/AGENTS.md"), "").unwrap();
        assert!(find_agents_md(empty.path()).is_empty());
    }

    /// A non-turn build pass carrying one step's tool activity — what
    /// `run_turn.rs` hands the injection pass after the step's calls ran.
    fn step(access: &StepAccess) -> InjectionContext<'_> {
        InjectionContext {
            is_new_turn: false,
            rearmed: false,
            injected: &[],
            access,
            history: &[],
        }
    }

    /// The reminder walks the chain from the workspace root down to each
    /// directory the previous step touched, so a nested `sub/AGENTS.md` is found
    /// when a tool reads `sub/deep/file.txt` — the reason v2 probes a chain
    /// rather than a single directory. Each file is suggested at most once, and
    /// one the model just read itself is not suggested back.
    #[test]
    fn test_agents_md_provider_walks_the_chain_and_suggests_once() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("sub").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_agents = nested.join("AGENTS.md");
        std::fs::write(&nested_agents, "# Nested").unwrap();

        let mut provider = agents_md_provider(Some(root.path().to_path_buf()), Vec::new());
        // A tool read `sub/deep/file.txt`; the reminder sees its parent.
        let access = StepAccess {
            dirs: vec![nested],
            ..Default::default()
        };
        let ctx = step(&access);

        let text = provider(&ctx).expect("the nested file is discovered");
        // v2 normalizes the paths it puts in the reminder, so the listed form is
        // forward-slashed even where the platform path is not.
        let expected = format!(
            "The following AGENTS.md file(s) apply to paths accessed by your recent tool \
             call, but were not included in your system prompt:\n- {}\n\
             Read them before making changes in those directories.",
            normalize_agents_md(&nested_agents)
        );
        assert_eq!(text, expected);
        assert_eq!(provider(&ctx), None, "at most once per agent");
    }

    /// The workspace root's own AGENTS.md is the head of every chain, so a tool
    /// touching any directory surfaces it — the case the previous
    /// workspace-root-only provider covered.
    #[test]
    fn test_agents_md_provider_still_finds_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        let agents_path = dir.path().join("AGENTS.md");
        std::fs::write(&agents_path, "# Instructions").unwrap();

        let mut provider = agents_md_provider(Some(dir.path().to_path_buf()), Vec::new());
        let access = StepAccess {
            dirs: vec![sub],
            ..Default::default()
        };
        let ctx = step(&access);

        let text = provider(&ctx).expect("the root file heads every chain");
        assert!(
            text.contains(&normalize_agents_md(&agents_path)),
            "content: {text}"
        );
    }

    /// v2 `selfKnown`: a model that just read an AGENTS.md is not told to go
    /// read it again.
    #[test]
    fn a_file_the_model_just_read_is_not_suggested_back() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let agents_path = sub.join("AGENTS.md");
        std::fs::write(&agents_path, "# Sub").unwrap();

        let mut provider = agents_md_provider(Some(root.path().to_path_buf()), Vec::new());
        let access = StepAccess {
            dirs: vec![sub.clone()],
            self_read: vec![agents_path],
            ..Default::default()
        };
        let ctx = step(&access);
        assert_eq!(provider(&ctx), None, "it already has the contents");

        // The suppression lasts one pass only (v2 clears `readRecently`).
        let later_access = StepAccess {
            dirs: vec![sub],
            ..Default::default()
        };
        let later = step(&later_access);
        assert!(
            provider(&later).is_some(),
            "a later tool call in the same subtree does remind"
        );
    }

    /// Each touched directory is probed against its own project root; an empty
    /// access or a chain with no AGENTS.md contributes nothing.
    #[test]
    fn agents_md_provider_probes_the_touched_dirs_own_root() {
        let nothing = StepAccess::default();
        let empty = step(&nothing);
        let mut no_root = agents_md_provider(None, Vec::new());
        assert_eq!(no_root(&empty), None);

        // v2 resolves each touched directory's own project root
        // (`findProjectRoot`), so a path outside the workspace still gets its
        // own AGENTS.md.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Instructions").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_agents = outside.path().join("AGENTS.md");
        std::fs::write(&outside_agents, "# Elsewhere").unwrap();
        let mut provider = agents_md_provider(Some(dir.path().to_path_buf()), Vec::new());
        let outside_access = StepAccess {
            dirs: vec![outside.path().to_path_buf()],
            ..Default::default()
        };
        let text = provider(&step(&outside_access)).expect("the outside file is discovered");
        assert!(
            text.contains(&normalize_agents_md(&outside_agents)),
            "content: {text}"
        );

        let bare = tempfile::tempdir().unwrap();
        let mut missing = agents_md_provider(Some(bare.path().to_path_buf()), Vec::new());
        assert_eq!(missing(&empty), None, "no AGENTS.md anywhere on the chain");
    }

    /// v2 `agentsMdReminderService.ts:255-262, 274-276`: a Bash call's own
    /// `cwd` argument is contributed whether or not any operand resolves, and
    /// it resolves against the agent's base directory when relative.
    #[test]
    fn a_bash_call_contributes_its_declared_cwd() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("tools");
        std::fs::create_dir_all(&sub).unwrap();
        let agents_path = sub.join("AGENTS.md");
        std::fs::write(&agents_path, "# Tools").unwrap();

        let mut provider = agents_md_provider(Some(root.path().to_path_buf()), Vec::new());
        let relative = StepAccess {
            declared_cwds: vec!["tools".to_string()],
            ..Default::default()
        };
        let text = provider(&step(&relative)).expect("the declared cwd heads a chain");
        assert!(
            text.contains(&normalize_agents_md(&agents_path)),
            "content: {text}"
        );

        // The same cwd spelled absolutely resolves to the same place.
        let mut absolute_provider = agents_md_provider(Some(root.path().to_path_buf()), Vec::new());
        let absolute = StepAccess {
            declared_cwds: vec![sub.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let text = absolute_provider(&step(&absolute)).expect("an absolute cwd resolves too");
        assert!(
            text.contains(&normalize_agents_md(&agents_path)),
            "content: {text}"
        );

        // A cwd outside the workspace is probed against its own project root;
        // this directory has no AGENTS.md, so it contributes nothing.
        let outside = tempfile::tempdir().unwrap();
        let mut outside_provider = agents_md_provider(Some(root.path().to_path_buf()), Vec::new());
        let escaping = StepAccess {
            declared_cwds: vec![outside.path().to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert_eq!(outside_provider(&step(&escaping)), None);
    }

    /// A path history already named starts out known, so a resumed session does
    /// not suggest it again — this is the fork's stand-in for v2's
    /// `origin.disclosure` seed.
    #[test]
    fn a_disclosed_path_is_never_suggested_again() {
        let dir = tempfile::tempdir().unwrap();
        let agents_path = dir.path().join("AGENTS.md");
        std::fs::write(&agents_path, "# Instructions").unwrap();
        let disclosed = vec![normalize_agents_md(&agents_path)];

        let mut provider = agents_md_provider(Some(dir.path().to_path_buf()), disclosed);
        let access = StepAccess {
            dirs: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        assert_eq!(provider(&step(&access)), None, "history already named it");
    }

    #[test]
    fn a_provider_that_renders_blank_injects_nothing() {
        let mut registry = InjectionRegistry::new();
        registry.register("blank", Box::new(|_| Some("   \n\t ".into())));
        registry.register("text", Box::new(|_| Some("content".into())));

        let texts = registry.build_injections(true);
        assert_eq!(texts, vec![wrap_system_reminder("content")]);
    }
}
