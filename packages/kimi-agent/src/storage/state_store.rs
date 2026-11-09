//! Local JSON state storage for the standalone REPL (P32 批 1).
//!
//! Provides per-domain durable state under
//! `<home>/.kimi-code/engine-state/<workspace-key>/state/` with one JSON file
//! per domain (`todo.json` / `plan.json` / `goal.json` / `cron.json` /
//! `task.json` / `turn.json`). The key is a digest of the canonicalized
//! workspace path, so the directory is not derivable from the workspace alone
//! — see [`read_workspace_state`] for the read side a host can call. Writes
//! are atomic (tmp file + rename) and serialized per domain. A domain file
//! that exists but fails to read or parse is **corrupt, not absent** — see
//! [`DomainRead`] — so an undo can never delete a file it could not read.
//!
//! Wire shapes align with the v2 state bridge domains: todo = `TodoItem[]`
//! (full replacement), plan = `{active, id?, path?}`, goal =
//! `{goal: <snapshot> | null}`, cron = entry list, task = task list, turn =
//! `{nextTurnId, cancelledTurnIds, anchorTurnIds, lastEnded?}`. The
//! action-shaped writes the engine submits (goal `create`/`update`/
//! `set_budget`, cron `create`/`delete`, task `stop`/`wait`) are applied
//! here with the v2 domain semantics, so the ported native tools render
//! their v2-aligned output against the local store.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::goal::{GoalBudgetLimits, GoalState, GoalStatus, to_snapshot};
use crate::primitives::read::{ReadOutcome, UnreadableCause, read_text};
use crate::turn_events::{TurnCancelTarget, TurnEvent};

/// The seven state domains the REPL persists locally.
pub const STATE_DOMAINS: [&str; 7] = ["todo", "plan", "goal", "cron", "task", "turn", "spec"];

/// The domains an undo does **not** rewind. v2 registers goal and cron
/// `undoable: false` (`goalService.ts`, `cronService.ts`): those participants
/// restore from the full journal, so cutting a turn must leave goal progress
/// and cron schedules at their latest value. Everything else is undoable.
pub const NON_UNDOABLE_STATE_DOMAINS: [&str; 2] = ["goal", "cron"];

/// Whether a domain takes part in undo checkpoints (v2 `undoable`).
pub fn is_undoable_state_domain(domain: &str) -> bool {
    !NON_UNDOABLE_STATE_DOMAINS.contains(&domain)
}

/// How many undo snapshots the checkpoint stack keeps, matching v2
/// `CHECKPOINT_MAX_TURNS` (`agent/checkpoint/checkpointService.ts:20`): undo
/// reaches at most this many turns back. Without the cap the stack grew one
/// file per turn for the life of the session.
pub const STATE_CHECKPOINT_MAX_ENTRIES: usize = 20;

/// The task output preview cap, matching the v2 `TASK_OUTPUT_PREVIEW_BYTES`.
pub const TASK_OUTPUT_PREVIEW_BYTES: usize = 32 * 1024;

/// The ceiling on a persisted task log, matching v2 `MAX_TASK_OUTPUT_BYTES`
/// (`agent/task/taskService.ts:151`) — the point at which v2 stops a
/// background process producing output rather than letting the log grow.
///
/// Same v2 number as `tools::MAX_TASK_OUTPUT_BYTES`, which is where the
/// producer stops. v2 has one constant for both because it streams the log;
/// this engine receives one finished `String`, so the two ceilings are
/// enforced in different places. **They must stay equal.**
pub const MAX_PERSISTED_TASK_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// The last [`MAX_PERSISTED_TASK_OUTPUT_BYTES`] of an output, cut on a
/// character boundary.
///
/// v2 keeps the **tail** everywhere it bounds output —
/// `appendRetainedOutput` drops whole chunks from the front
/// (`taskService.ts:1001-1019`) and `readTaskOutputSnapshot` slices
/// `output.data.byteLength - previewBytes` (`:137-138`). The end of a
/// command's output is the part that answers what happened; the beginning is
/// what it started from.
fn cap_output_tail(output: &str) -> &str {
    if output.len() <= MAX_PERSISTED_TASK_OUTPUT_BYTES {
        return output;
    }
    let mut start = output.len() - MAX_PERSISTED_TASK_OUTPUT_BYTES;
    // `floor_char_boundary` is unstable; stepping to the next boundary is the
    // same cut and needs no unsafe.
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    &output[start..]
}

/// The JSON shape name for an error that must say what it found instead of
/// the list it expected.
pub(crate) fn json_shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// The result of applying a domain write: the value to persist and the
/// value to return to the caller (the v2 host response value). They differ
/// for the action-shaped domains — cron create returns the created entry
/// while the stored value is the full list, task stop returns the stopped
/// entry — mirroring the v2 `state_write` adapter.
#[derive(Debug)]
pub struct StateWriteOutcome {
    pub stored: Value,
    pub response: Value,
}

/// What a domain file on disk actually holds: present, genuinely absent, or
/// unreadable. `Absent` and `Unreadable` must stay distinguishable — collapsing
/// both into "no value" is what let a truncated `todo.json` read as "no todos",
/// get frozen into a checkpoint as `None`, and then be deleted by the next undo.
/// v2 refuses the ambiguity outright: its atomic document store raises
/// `STORAGE_DECODE_FAILED` instead of returning `undefined` (v2
/// `persistence/backends/node-fs/atomicDocumentStore.ts`).
///
/// This is [`ReadOutcome<Value>`] rather than a storage-local enum so the same
/// three states mean the same thing at every read site in the engine.
pub type DomainRead = ReadOutcome<Value>;

/// Local per-domain JSON state store.
/// A full snapshot of every state domain, taken at a turn checkpoint.
/// `None` means the domain had no stored value at snapshot time (rollback
/// clears it back to absent) — and, since [`StateStore::checkpoint`] refuses
/// to freeze a corrupt domain, it can only mean the domain really was absent.
type StateSnapshot = Vec<(String, Option<Value>)>;

pub struct StateStore {
    state_dir: PathBuf,
    /// Undo checkpoint stack directory (v2 undo-anchor semantics): one file
    /// per snapshot under `state_dir/checkpoints/<seq>.json`. The stack
    /// survives restart — `rollback` reads the highest-numbered file,
    /// restores, and deletes it. Pushed by `checkpoint` at every turn head
    /// and consumed by `rollback` on undo.
    checkpoints_dir: PathBuf,
    /// Serializes concurrent `checkpoint` / `rollback` calls so the stack
    /// numbering and the per-file renames are race-free.
    checkpoint_lock: Mutex<()>,
    /// One write lock per domain. The per-turn server pipeline and the task
    /// runner are separate tasks in the same process writing the same domain
    /// files; without this they interleave their writes and renames.
    domain_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl StateStore {
    /// Create (or open) the state store under an explicit directory.
    /// Tests use this directly; production resolves the directory through
    /// [`for_workspace`].
    pub fn for_dir(state_dir: PathBuf) -> std::io::Result<Self> {
        let checkpoints_dir = state_dir.join("checkpoints");
        fs::create_dir_all(&checkpoints_dir)?;
        Ok(Self {
            state_dir,
            checkpoints_dir,
            checkpoint_lock: Mutex::new(()),
            domain_locks: Mutex::new(HashMap::new()),
        })
    }

    /// Create (or open) the state store for a workspace, under the
    /// engine-local root: `~/.kimi-code/engine-state/<workspace-key>/state/`.
    pub fn for_workspace(workspace_root: &Path) -> std::io::Result<Self> {
        Self::for_dir(super::paths::engine_state_dir(workspace_root)?.join("state"))
    }

    /// Create (or open) the state store for one session inside a workspace:
    /// `~/.kimi-code/engine-state/<workspace-key>/sessions/<session-key>/state/`.
    ///
    /// v2 scopes the agent domains to the agent (`todo` / `plan` / `goal` /
    /// `task` / `turn`); one workspace-level store shared them across every
    /// session of the checkout. `cron` stays workspace-level — a schedule is
    /// created for the workspace and fires regardless of which session made it
    /// — so callers route that domain to [`Self::for_workspace`].
    pub fn for_session(workspace_root: &Path, session_id: &str) -> std::io::Result<Self> {
        Self::for_dir(super::paths::engine_session_dir(workspace_root, session_id)?.join("state"))
    }

    /// Snapshot every undoable domain (v2 undo-anchor checkpoint). The
    /// snapshot is persisted to a numbered file under `checkpoints/`, so undo
    /// survives a restart. A file that exists but fails to read/parse is an
    /// error, not an absent domain — rolling back must never silently wipe
    /// data, so a corrupt domain makes the whole checkpoint fail instead of
    /// freezing `None` for a domain whose bytes still exist.
    pub fn checkpoint(&self) -> Result<(), String> {
        let _guard = self
            .checkpoint_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut snapshot = Vec::with_capacity(STATE_DOMAINS.len());
        for domain in STATE_DOMAINS
            .into_iter()
            .filter(|domain| is_undoable_state_domain(domain))
        {
            let entry = match self.read_domain_state(domain) {
                ReadOutcome::Found(value) => Some(value),
                ReadOutcome::Absent => None,
                ReadOutcome::Unreadable(cause) => {
                    return Err(format!(
                        "state domain '{domain}' is unreadable ({cause}); \
                         refusing to checkpoint it as absent — repair or move \
                         {} first",
                        self.domain_file(domain)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| format!("{domain}.json"))
                    ));
                }
            };
            snapshot.push((domain.to_string(), entry));
        }
        let next = next_checkpoint_seq(&self.checkpoints_dir)?;
        write_checkpoint_file(&self.checkpoints_dir, next, &snapshot)?;
        // Keep the stack inside the v2 window. The new snapshot is already
        // durable, so a failure here does not fail the checkpoint — but it is
        // never silent: a leftover snapshot file is disk the caller believes
        // was reclaimed.
        match prune_checkpoint_files(&self.checkpoints_dir, STATE_CHECKPOINT_MAX_ENTRIES) {
            Ok(0) => {}
            Ok(removed) => tracing::debug!(removed, "pruned old state checkpoints"),
            Err(error) => tracing::warn!(
                %error,
                "the checkpoint was written but older snapshots could not be pruned; they remain on disk"
            ),
        }
        Ok(())
    }

    /// Restore the most recent checkpoint (highest-numbered file under
    /// `checkpoints/`). Returns `Ok(false)` when there is nothing to undo.
    ///
    /// A snapshot entry of `None` means "the domain was absent when the
    /// snapshot was taken", so restoring it clears the file. If the file is
    /// there but unreadable now, it is the user's only copy and rollback
    /// leaves it alone (and says so) rather than deleting it.
    pub fn rollback(&self) -> Result<bool, String> {
        let _guard = self
            .checkpoint_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(seq) = latest_checkpoint_seq(&self.checkpoints_dir)? else {
            return Ok(false);
        };
        let path = checkpoint_path(&self.checkpoints_dir, seq);
        let content =
            fs::read_to_string(&path).map_err(|e| format!("checkpoint read seq {seq}: {e}"))?;
        let snapshot: StateSnapshot = serde_json::from_str(&content)
            .map_err(|e| format!("checkpoint parse seq {seq}: {e}"))?;
        for (domain, value) in snapshot {
            // Snapshots written before goal/cron were excluded still carry
            // them; restoring those entries would rewind progress undo must
            // keep, so non-undoable domains are skipped on the way out too.
            if !is_undoable_state_domain(&domain) {
                continue;
            }
            match value {
                Some(v) => self.write_domain(&domain, &v)?,
                None => {
                    if let ReadOutcome::Unreadable(cause) = self.read_domain_state(&domain) {
                        tracing::warn!(
                            domain = %domain,
                            %cause,
                            "rollback left an unreadable state domain in place instead of deleting it"
                        );
                        continue;
                    }
                    self.clear_domain(&domain)?;
                }
            }
        }
        fs::remove_file(&path).map_err(|e| format!("checkpoint remove seq {seq}: {e}"))?;
        Ok(true)
    }

    /// How many checkpoints are pending (v2 `checkpointDepth`).
    ///
    /// A stack directory that cannot be listed reports zero — the callers (a
    /// host badge, the napi export) have no error channel — but the failure is
    /// logged: "0" and "could not count" are different answers, and only one of
    /// them means the user has nothing to undo.
    pub fn checkpoint_depth(&self) -> usize {
        match count_checkpoint_files(&self.checkpoints_dir) {
            Ok(depth) => depth,
            Err(error) => {
                tracing::warn!(
                    dir = %self.checkpoints_dir.display(),
                    "could not count the undo checkpoints; reporting zero depth: {error}"
                );
                0
            }
        }
    }

    /// The state directory (`<workspace>/.kimi/state`).
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn domain_file(&self, domain: &str) -> Option<PathBuf> {
        STATE_DOMAINS
            .contains(&domain)
            .then(|| self.state_dir.join(format!("{domain}.json")))
    }

    /// The write lock for one domain, created on first use. Held for the whole
    /// write (temp file → fsync → rename), so two writers of the same domain
    /// in one process cannot truncate or reorder each other's output.
    fn domain_lock(&self, domain: &str) -> Arc<Mutex<()>> {
        let mut locks = self.domain_locks.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            locks
                .entry(domain.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Read a domain's stored value, distinguishing the three on-disk states.
    /// `Absent` covers both "no file yet" and an unknown domain name.
    pub fn read_domain_state(&self, domain: &str) -> DomainRead {
        let Some(path) = self.domain_file(domain) else {
            return ReadOutcome::Absent;
        };
        let content = match read_text(&path) {
            ReadOutcome::Found(content) => content,
            ReadOutcome::Absent => return ReadOutcome::Absent,
            ReadOutcome::Unreadable(cause) => return ReadOutcome::Unreadable(cause),
        };
        match serde_json::from_str(&content) {
            Ok(value) => ReadOutcome::Found(value),
            Err(error) => ReadOutcome::Unreadable(UnreadableCause::new(
                format!("parse {}", path.display()),
                error,
            )),
        }
    }

    /// Write a domain's value atomically (tmp file + rename, so a crash
    /// mid-write never leaves a truncated domain file behind). Serialized
    /// against other writers of the same domain.
    pub fn write_domain(&self, domain: &str, value: &Value) -> Result<(), String> {
        let lock = self.domain_lock(domain);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        self.write_domain_locked(domain, value)
    }

    /// [`Self::write_domain`] without taking the domain lock.
    ///
    /// The caller already holds the lock for a whole read-modify-write. Taking
    /// it again would deadlock (the lock is not reentrant), and taking it late
    /// would reopen the lost-update window the locked read-modify-write exists
    /// to close.
    fn write_domain_locked(&self, domain: &str, value: &Value) -> Result<(), String> {
        let path = self
            .domain_file(domain)
            .ok_or_else(|| format!("unknown state domain: {domain}"))?;
        let json = serde_json::to_string_pretty(value)
            .map_err(|e| format!("serialize {domain} state: {e}"))?;
        atomic_replace(&path, json.as_bytes())
    }

    /// Remove a domain's stored state. Serialized against writers of the
    /// same domain so a write cannot resurrect the file after the unlink.
    pub fn clear_domain(&self, domain: &str) -> Result<(), String> {
        let path = self
            .domain_file(domain)
            .ok_or_else(|| format!("unknown state domain: {domain}"))?;
        let lock = self.domain_lock(domain);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if path.is_file() {
            fs::remove_file(&path).map_err(|e| format!("remove {domain} state: {e}"))?;
        }
        Ok(())
    }

    /// Apply an RFC 6902 JSON patch to a domain's state.
    /// Returns the updated domain value and the inverse patch for undo.
    pub fn patch_domain(
        &self,
        domain: &str,
        patch: &crate::session::patch::JsonPatchSet,
    ) -> Result<(Value, crate::session::patch::JsonPatchSet), String> {
        // The read and the write must be one transaction: a patch is computed
        // from the value that was read, so a writer landing between the two
        // would be silently overwritten.
        let lock = self.domain_lock(domain);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut val = match self.read_domain_state(domain) {
            ReadOutcome::Found(value) => value,
            ReadOutcome::Absent => Self::default_value(domain).unwrap_or(Value::Null),
            ReadOutcome::Unreadable(cause) => {
                return Err(format!(
                    "cannot patch state domain '{domain}': it is unreadable ({cause}); \
                     patching would overwrite the only copy of the file"
                ));
            }
        };
        let inverse = crate::session::patch::apply_patch(&mut val, patch)
            .map_err(|e| format!("failed to apply patch to domain '{domain}': {e}"))?;
        self.write_domain_locked(domain, &val)?;
        Ok((val, inverse))
    }

    /// Compute RFC 6902 JSON diff between current domain state and a new value.
    ///
    /// An unreadable domain is an error, not an empty one: the diff computed
    /// from "nothing" would tell the caller to add every field, hiding the
    /// state that is still on disk.
    pub fn diff_domain(
        &self,
        domain: &str,
        new_value: &Value,
    ) -> Result<crate::session::patch::JsonPatchSet, String> {
        let current = match self.read_domain_state(domain) {
            ReadOutcome::Found(value) => value,
            ReadOutcome::Absent => Self::default_value(domain).unwrap_or(Value::Null),
            ReadOutcome::Unreadable(cause) => {
                return Err(format!(
                    "cannot diff state domain '{domain}': it is unreadable ({cause})"
                ));
            }
        };
        Ok(crate::session::patch::diff_values(&current, new_value))
    }

    /// The v2-aligned default wire value for a domain with no stored state.
    pub fn default_value(domain: &str) -> Option<Value> {
        match domain {
            "todo" => Some(json!([])),
            "plan" => Some(json!({ "active": false })),
            "goal" => Some(json!({ "goal": null })),
            "cron" => Some(json!([])),
            "task" => Some(json!([])),
            "turn" => Some(json!({
                "nextTurnId": 0u64,
                "cancelledTurnIds": [],
                "anchorTurnIds": [],
            })),
            // v2 `specKey` default (`specOps.ts:84`): every session starts
            // with spec mode off, exactly like plan.
            "spec" => Some(json!({ "active": false })),
            _ => None,
        }
    }

    /// Read a domain's state through the bridge protocol: the stored value,
    /// the domain default when none is stored, or a `-32001` error for an
    /// unknown domain. The task domain reads a single task's entry when
    /// `key` is a task id (TaskOutput) and the full list when `key ==
    /// "task"` (TaskList); an unknown task id is a `-32002` error.
    pub fn read_state(&self, domain: &str, key: &str) -> Result<Value, String> {
        if domain == "task" && key != "task" {
            let tasks = match self.read_domain_state("task") {
                ReadOutcome::Found(Value::Array(tasks)) => tasks,
                // A file that parses but is not a list is malformed, not
                // empty: reading it as empty would answer "-32002 Task not
                // found" for a task whose bytes are still on disk.
                ReadOutcome::Found(other) => {
                    return Err(format!(
                        "State read error: task state is malformed: expected a list of entries, found {}",
                        json_shape(&other)
                    ));
                }
                ReadOutcome::Absent => Vec::new(),
                ReadOutcome::Unreadable(cause) => {
                    return Err(format!("State read error: {cause}"));
                }
            };
            let mut entry = tasks
                .iter()
                .find(|t| t.get("taskId").and_then(|i| i.as_str()) == Some(key))
                .cloned()
                .ok_or_else(|| format!("State read error: [-32002] Task not found: {key}"))?;
            if let Some(snapshot) = self.read_task_output(key, TASK_OUTPUT_PREVIEW_BYTES)
                && let Some(obj) = entry.as_object_mut()
                && let Some(fields) = snapshot.as_object()
            {
                for (field, value) in fields {
                    obj.insert(field.clone(), value.clone());
                }
            }
            return Ok(entry);
        }
        match self.read_domain_state(domain) {
            ReadOutcome::Found(value) => Ok(value),
            ReadOutcome::Absent => Self::default_value(domain).ok_or_else(|| {
                format!("State read error: [-32001] unknown state domain: {domain}")
            }),
            ReadOutcome::Unreadable(cause) => Err(format!("State read error: {cause}")),
        }
    }

    /// Apply the v2 domain write semantics to `value` and return the
    /// post-write outcome (the caller persists `stored` and returns
    /// `response`). Unknown domains are `-32001` errors; invalid values and
    /// rejected writes carry the v2 `-32003` / `-32004` codes so the
    /// engine's error mapping stays intact.
    pub fn apply_write(&self, domain: &str, value: &Value) -> Result<StateWriteOutcome, String> {
        match domain {
            "todo" => {
                if !value.is_array() {
                    return Err(
                        "State write error: [-32003] invalid todo state value: expected an array of todo items".into(),
                    );
                }
                Ok(StateWriteOutcome {
                    stored: value.clone(),
                    response: value.clone(),
                })
            }
            "plan" => self.apply_plan_write(value),
            "goal" => self.apply_goal_write(value),
            "cron" => self.apply_cron_write(value),
            "task" => self.apply_task_write(value),
            "turn" => self.apply_turn_write(value),
            "spec" => self.apply_spec_write(value),
            _ => Err(format!(
                "State write error: [-32001] unknown state domain: {domain}"
            )),
        }
    }

    /// Apply a domain write and persist it **inside one domain lock**.
    ///
    /// [`Self::apply_write`] returns a value for the caller to persist, which
    /// leaves a window between the read-modify-write it performed and the
    /// caller's [`Self::write_domain`]. The task runner and the turn pipeline
    /// share the goal / task / cron domains, so a writer can land in that
    /// window and be overwritten. Production write paths use this method so the
    /// whole transaction is serialized; `apply_write` stays public for callers
    /// that only need the computed value.
    pub fn apply_write_and_persist(
        &self,
        domain: &str,
        value: &Value,
    ) -> Result<StateWriteOutcome, String> {
        let lock = self.domain_lock(domain);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = self.apply_write(domain, value)?;
        self.write_domain_locked(domain, &outcome.stored)?;
        Ok(outcome)
    }

    /// Read a domain whose wire shape is a list.
    ///
    /// A file that parses but holds another shape is an error, not an empty
    /// list: `as_array().cloned().unwrap_or_default()` read a wrong-shaped
    /// `cron.json` / `task.json` as "no entries", and the next write replaced
    /// it — destroying every entry the file still held.
    fn read_domain_list(&self, domain: &str) -> Result<Vec<Value>, String> {
        match self.read_domain_state(domain) {
            ReadOutcome::Found(Value::Array(entries)) => Ok(entries),
            ReadOutcome::Found(other) => Err(format!(
                "State write error: {domain} state is malformed: expected a list of entries, found {}",
                json_shape(&other)
            )),
            ReadOutcome::Absent => Ok(Vec::new()),
            ReadOutcome::Unreadable(cause) => Err(format!(
                "State write error: {domain} state is unreadable ({cause}); \
                 writing it would overwrite the existing file"
            )),
        }
    }

    fn apply_turn_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let next_turn_id = value
            .get("nextTurnId")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                "State write error: [-32003] invalid turn state value: missing nextTurnId"
                    .to_string()
            })?;
        let mut state = match self.read_domain_state("turn") {
            ReadOutcome::Found(Value::Object(map)) => map,
            ReadOutcome::Found(_) | ReadOutcome::Absent => serde_json::Map::new(),
            ReadOutcome::Unreadable(cause) => {
                return Err(format!(
                    "State write error: turn state is unreadable ({cause}); \
                     writing it would overwrite the turn clock"
                ));
            }
        };
        // The clock never rewinds: a lower value would hand out an id that
        // durable turn events have already been recorded against.
        let stored = state
            .get("nextTurnId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        state.insert("nextTurnId".into(), json!(next_turn_id.max(stored)));
        for field in ["cancelledTurnIds", "anchorTurnIds", "lastEnded"] {
            match value.get(field) {
                Some(incoming) => {
                    state.insert(field.into(), incoming.clone());
                }
                None => {
                    state.remove(field);
                }
            }
        }
        let stored = Value::Object(state);
        Ok(StateWriteOutcome {
            stored: stored.clone(),
            response: stored,
        })
    }

    /// Fold one engine turn lifecycle event into the persisted turn clock —
    /// the REPL host's stand-in for v2's `turnKey` fold (`turnOps.ts:66-160`).
    /// `turn.started` is observable-only and changes nothing.
    ///
    /// `anchorTurnIds` follows v2's `isUndoAnchorOrigin`: a user turn records
    /// its id so an undo can cut back to it, while a continuation (goal, cron,
    /// task wake) does not. `lastEnded` is cleared as soon as a newer turn
    /// appends to the context or is claimed, exactly like v2's
    /// `ContextAppendLoopEvent` / `TurnPrompt` branches.
    pub fn fold_turn_event(&self, event: &TurnEvent) {
        // The fold is a read-modify-write of one file: the clock advance, the
        // new anchor and the cleared `lastEnded` are all computed from what was
        // read. Taking the domain lock only for the write would let another
        // writer land between the two and be overwritten — the turn clock would
        // then reissue an id the transcript already records.
        let lock = self.domain_lock("turn");
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = match self.read_domain_state("turn") {
            ReadOutcome::Found(Value::Object(map)) => map,
            ReadOutcome::Found(_) | ReadOutcome::Absent => serde_json::Map::new(),
            // This fold runs from the synchronous turn-event hook, which has no
            // error channel. Folding an unreadable clock into an empty one
            // would then be written back over the only copy, so the fold is
            // skipped and the failure is logged instead.
            ReadOutcome::Unreadable(cause) => {
                tracing::warn!(
                    %cause,
                    "turn state is unreadable; skipping this turn-event fold to keep the file intact"
                );
                return;
            }
        };
        let next_turn_id = state
            .get("nextTurnId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let last_ended_id = state
            .get("lastEnded")
            .and_then(|v| v.get("turnId"))
            .and_then(|v| v.as_u64());
        // v2 clears `lastEnded` once a turn newer than it touches the context:
        // the summary would otherwise claim the session ended where it did not.
        let clear_stale_last_ended = |state: &mut serde_json::Map<String, Value>, turn_id: u64| {
            if last_ended_id.is_some_and(|ended| turn_id > ended) {
                state.remove("lastEnded");
            }
        };
        // v2 `advanceTurnClock` (`turnOps.ts:203-215`): the clock consumes the
        // cancellations it steps over, so `cancelledTurnIds` only ever names
        // ids still ahead of it. Without this the list grows forever and the
        // persisted state drifts from the reference.
        let advance_turn_clock = |state: &mut serde_json::Map<String, Value>, mut next: u64| {
            let mut cancelled: Vec<u64> = state
                .get("cancelledTurnIds")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect())
                .unwrap_or_default();
            cancelled.retain(|id| *id >= next);
            while let Some(index) = cancelled.iter().position(|id| *id == next) {
                cancelled.remove(index);
                next += 1;
            }
            cancelled.sort_unstable();
            state.insert("cancelledTurnIds".into(), json!(cancelled));
            state.insert("nextTurnId".into(), json!(next));
        };
        match event {
            TurnEvent::Prompt {
                turn_id, origin, ..
            } => {
                clear_stale_last_ended(&mut state, *turn_id);
                if crate::session::origin_is_undo_anchor(origin) {
                    let mut anchors = state
                        .get("anchorTurnIds")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect::<Vec<u64>>())
                        .unwrap_or_default();
                    anchors.push(*turn_id);
                    state.insert("anchorTurnIds".into(), json!(anchors));
                }
                advance_turn_clock(&mut state, next_turn_id.max(turn_id + 1));
            }
            TurnEvent::Cancel {
                turn_id, target, ..
            } => {
                // A cancellation aimed at the active turn is accounted for by
                // turn.ended; only a queued one reserves an id.
                if target.is_some_and(|t| t != TurnCancelTarget::Queued) {
                    return;
                }
                let Some(turn_id) = *turn_id else {
                    return;
                };
                if turn_id < next_turn_id {
                    return;
                }
                let mut cancelled = state
                    .get("cancelledTurnIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_u64()).collect::<Vec<u64>>())
                    .unwrap_or_default();
                if !cancelled.contains(&turn_id) {
                    cancelled.push(turn_id);
                }
                state.insert("cancelledTurnIds".into(), json!(cancelled));
                advance_turn_clock(&mut state, next_turn_id);
            }
            TurnEvent::Ended {
                turn_id,
                reason,
                duration_ms,
                ..
            } => {
                state.insert(
                    "lastEnded".into(),
                    json!({ "turnId": turn_id, "reason": reason, "durationMs": duration_ms }),
                );
            }
            TurnEvent::Started { .. } => return,
            // A steer joins an already-counted turn, but it proves that turn
            // appended to the context after the last ended one.
            TurnEvent::Steer { turn_id, .. } => match turn_id {
                Some(turn_id) => clear_stale_last_ended(&mut state, *turn_id),
                None => return,
            },
        }
        if let Err(error) = self.write_domain_locked("turn", &Value::Object(state)) {
            tracing::warn!(%error, "could not persist the folded turn event");
        }
    }

    /// Forget the undo anchors and the last-ended summary, as v2's
    /// `ContextClear` / `ContextApplyCompaction` branches do: after the
    /// context was rewritten, an anchor from before the rewrite no longer
    /// names a cut point. The turn clock and cancelled ids stay.
    pub fn clear_turn_anchors(&self) {
        let lock = self.domain_lock("turn");
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let state = match self.read_domain_state("turn") {
            ReadOutcome::Found(Value::Object(mut map)) => {
                map.remove("anchorTurnIds");
                map.remove("lastEnded");
                Value::Object(map)
            }
            _ => return,
        };
        if let Err(error) = self.write_domain_locked("turn", &state) {
            tracing::warn!(%error, "could not clear the turn anchors");
        }
    }

    fn apply_plan_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(active) = value.get("active").and_then(|v| v.as_bool()) else {
            return Err(
                "State write error: [-32003] invalid plan state value: expected { active: boolean }"
                    .into(),
            );
        };
        if !active {
            let stored = json!({ "active": false });
            return Ok(StateWriteOutcome {
                stored: stored.clone(),
                response: stored,
            });
        }
        // Enter: the host generates the plan id and file path (v2
        // `PlanModeEnter`); an already-active plan rejects the write.
        let already_active = match self.read_domain_state("plan") {
            ReadOutcome::Found(plan) => plan.get("active").and_then(|v| v.as_bool()) == Some(true),
            ReadOutcome::Absent => false,
            ReadOutcome::Unreadable(cause) => {
                return Err(format!(
                    "State write error: plan state is unreadable ({cause}); \
                     entering plan mode would overwrite it"
                ));
            }
        };
        if already_active {
            return Err("State write error: [-32004] plan mode is already active".into());
        }
        let id = format!("plan-{:016x}", fastrand::u64(..));
        let plans_dir = self
            .state_dir
            .parent()
            .unwrap_or(&self.state_dir)
            .join("plans");
        fs::create_dir_all(&plans_dir)
            .map_err(|e| format!("create plan directory {}: {e}", plans_dir.display()))?;
        let path = plans_dir.join(format!("{id}.md"));
        // v2 `writeEmptyPlanFile`: the plan file exists from the moment plan
        // mode is entered, so the model can Read it before writing.
        fs::write(&path, "").map_err(|e| format!("create plan file {}: {e}", path.display()))?;
        let stored = json!({
            "active": true,
            "id": id,
            "path": path.to_string_lossy(),
        });
        Ok(StateWriteOutcome {
            stored: stored.clone(),
            response: stored,
        })
    }

    /// v2 `specKey` writes (`specOps.ts:87-102`). The state shape is
    /// `{active, id?, lastTransition?}`. `id` is generated by the engine's
    /// `EnterSpecMode`, not the store: the transition needs the id *before* the
    /// write so it can create `<workspace>/specs/<id>` first (v2
    /// `AgentSpecService.enter`). An already-active state is accepted here on
    /// purpose: the tool enforces "already in spec mode", while the mode mutex
    /// needs an exact restore after a failed switch (v2 `switchTo` re-enters
    /// the previous mode on failure).
    fn apply_spec_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let active = value
            .get("active")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                "State write error: [-32003] invalid spec state value: expected { active: boolean }"
                    .to_string()
            })?;
        if !active {
            let mut stored = json!({ "active": false });
            if let Some(transition) = value.get("lastTransition").and_then(Value::as_str) {
                stored["lastTransition"] = json!(transition);
            }
            return Ok(StateWriteOutcome {
                stored: stored.clone(),
                response: stored,
            });
        }
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                "State write error: [-32003] invalid spec state value: entering spec mode needs a non-empty id"
                    .to_string()
            })?;
        let stored = json!({ "active": true, "id": id });
        Ok(StateWriteOutcome {
            stored: stored.clone(),
            response: stored,
        })
    }

    fn apply_goal_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(action) = value.get("action").and_then(|a| a.as_str()) else {
            return Err(
                "State write error: [-32003] invalid goal state value: expected { action: \"create\" | \"update\" | \"set_budget\" | \"record_usage\" | \"lifecycle\", ... }".into(),
            );
        };
        match action {
            "create" => self.goal_create(value),
            "objective" => self.goal_edit_objective(value),
            "update" => self.goal_update(value),
            "set_budget" => self.goal_set_budget(value),
            // v2 `incrementGoalTurn` + `accountTokenUsage`: the turn-end fold
            // the turn driver submits, so an active goal's counters and the
            // continuation budget stay current without host arithmetic.
            "record_usage" => self.goal_record_usage_action(value),
            // v2 `applyLifecycle`: the runtime-side transitions the engine
            // itself drives (pause on interrupt / error, block on budget).
            // They only apply while the goal is active, unlike the
            // user-facing `update` action.
            "lifecycle" => self.goal_lifecycle(value),
            _ => Err(format!(
                "State write error: [-32003] invalid goal action: {action}"
            )),
        }
    }

    fn goal_record_usage_action(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let turns = value.get("turns").and_then(Value::as_u64).unwrap_or(1);
        let tokens = value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let now = now_ms();
        let Some(mut state) = self.read_goal_state()? else {
            // Every finished turn submits this fold; a session without a goal
            // is the common case, not an error.
            return Ok(StateWriteOutcome {
                stored: json!({ "goal": Value::Null }),
                response: json!({ "goal": Value::Null }),
            });
        };
        if state.status == GoalStatus::Active {
            state.turns_used += turns;
            state.tokens_used += tokens;
            state.output_tokens_used += tokens;
            state.updated_at = now;
            // v2 `accountTokenUsage` / `incrementGoalTurn` end with
            // `blockIfBudgetReached`: the write that spends the last budget
            // unit is the one that blocks, so the caller observes the block
            // in this same response.
            if let Some(outcome) = self.block_if_budget_reached(&mut state, now) {
                return Ok(outcome);
            }
        }
        Ok(goal_outcome(&state, now))
    }

    /// v2 `applyLifecycle`: settle the wall clock, set the status and reason,
    /// and anchor (or clear) the live wall-clock start. Pausing / blocking /
    /// completing is a no-op once the goal already left `active`
    /// (`pauseActiveGoal` / `markBlocked` / `markComplete`); resuming is only
    /// legal from `paused` / `blocked` (`resumeGoal`).
    fn goal_lifecycle(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(status) = value.get("status").and_then(|s| s.as_str()) else {
            return Err(
                "State write error: [-32003] invalid goal lifecycle value: expected { action: \"lifecycle\", status: string, reason?: string }".into(),
            );
        };
        let reason = value.get("reason").and_then(|r| r.as_str());
        let now = now_ms();
        let Some(mut state) = self.read_goal_state()? else {
            return Ok(StateWriteOutcome {
                stored: json!({ "goal": Value::Null }),
                response: json!({ "goal": Value::Null }),
            });
        };
        match status {
            "active" => {
                if !matches!(state.status, GoalStatus::Paused | GoalStatus::Blocked) {
                    return Err(format!(
                        "State write error: [-32004] Cannot resume a goal in status \"{}\"",
                        state.status.as_str()
                    ));
                }
                settle_wall_clock(&mut state, now);
                state.status = GoalStatus::Active;
                state.terminal_reason = None;
                state.wall_clock_resumed_at = Some(now);
                state.updated_at = now;
                // v2 `resumeGoal` blocks immediately when the resumed goal has
                // no budget left instead of buying another turn.
                if let Some(outcome) = self.block_if_budget_reached(&mut state, now) {
                    return Ok(outcome);
                }
            }
            "paused" | "blocked" | "complete" => {
                let target = match status {
                    "paused" => GoalStatus::Paused,
                    "blocked" => GoalStatus::Blocked,
                    _ => GoalStatus::Complete,
                };
                if state.status != GoalStatus::Active {
                    return Ok(goal_outcome(&state, now));
                }
                settle_wall_clock(&mut state, now);
                state.status = target;
                state.terminal_reason = reason.map(str::to_string);
                state.wall_clock_resumed_at = None;
                state.updated_at = now;
            }
            _ => {
                return Err(format!(
                    "State write error: [-32003] invalid goal lifecycle status: {status}"
                ));
            }
        }
        Ok(goal_outcome(&state, now))
    }

    /// v2 `blockIfBudgetReached`: an active goal whose usage reached any limit
    /// moves to `blocked` with the v2 reason, settling the wall clock first so
    /// the stored elapsed time stops at the block.
    fn block_if_budget_reached(
        &self,
        state: &mut GoalState,
        now: u64,
    ) -> Option<StateWriteOutcome> {
        if state.status != GoalStatus::Active {
            return None;
        }
        let snapshot = to_snapshot(state, live_wall_clock_ms(state, now));
        let reason = crate::goal::goal_budget_block_reason(&snapshot.budget)?;
        settle_wall_clock(state, now);
        state.status = GoalStatus::Blocked;
        state.terminal_reason = Some(reason);
        state.wall_clock_resumed_at = None;
        state.updated_at = now;
        Some(goal_outcome(state, now))
    }

    fn goal_create(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(objective) = value.get("objective").and_then(|o| o.as_str()) else {
            return Err(
                "State write error: [-32003] invalid goal create value: expected { action: \"create\", objective: string, completion_criterion?: string }".into(),
            );
        };
        if self.read_goal_state()?.is_some() {
            return Err(
                "State write error: [-32004] A goal already exists; use replace to start a new one"
                    .into(),
            );
        }
        let now = now_ms();
        let state = GoalState {
            goal_id: format!("goal-{:016x}", fastrand::u64(..)),
            objective: objective.to_string(),
            completion_criterion: value
                .get("completion_criterion")
                .and_then(|c| c.as_str())
                .map(str::to_string),
            status: GoalStatus::Active,
            turns_used: 0,
            tokens_used: 0,
            input_tokens_used: 0,
            output_tokens_used: 0,
            wall_clock_ms: 0,
            wall_clock_resumed_at: Some(now),
            budget_limits: GoalBudgetLimits::default(),
            terminal_reason: None,
            host_owned: false,
            created_at: now,
            updated_at: now,
        };
        Ok(goal_outcome(&state, now))
    }

    /// v2 #4568 `editGoalObjective`: change only the objective of the current
    /// goal. A no-op still returns the live snapshot so the route answers with
    /// the same shape as a real edit.
    fn goal_edit_objective(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(objective) = value.get("objective").and_then(|o| o.as_str()) else {
            return Err(
                "State write error: [-32003] invalid goal objective value: expected { action: \"objective\", objective: string }".into(),
            );
        };
        let now = now_ms();
        let Some(mut state) = self.read_goal_state()? else {
            return Ok(StateWriteOutcome {
                stored: json!({ "goal": Value::Null }),
                response: json!({ "goal": Value::Null }),
            });
        };
        state.edit_objective(objective)?;
        state.updated_at = now;
        Ok(goal_outcome(&state, live_wall_clock_ms(&state, now)))
    }

    fn goal_update(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(status) = value.get("status").and_then(|s| s.as_str()) else {
            return Err(
                "State write error: [-32003] invalid goal update value: expected { action: \"update\", status: \"active\" | \"paused\" | \"complete\" | \"blocked\", reason?: string }".into(),
            );
        };
        if !matches!(status, "active" | "paused" | "complete" | "blocked") {
            return Err(
                "State write error: [-32003] Invalid goal status. Use `active`, `complete`, or `blocked`.".into(),
            );
        }
        let reason = value.get("reason").and_then(|r| r.as_str());
        let Some(mut state) = self.read_goal_state()? else {
            let message = match status {
                "active" => "No current goal",
                "paused" => "No current goal",
                "complete" => "Goal not completed: no active goal.",
                _ => "Goal not blocked: no active goal.",
            };
            return Err(format!("State write error: [-32004] {message}"));
        };
        let now = now_ms();
        match status {
            "active" => {
                if state.status == GoalStatus::Active {
                    return Ok(goal_outcome(&state, now));
                }
                if !matches!(state.status, GoalStatus::Paused | GoalStatus::Blocked) {
                    return Err(format!(
                        "State write error: [-32004] Cannot resume a goal in status \"{}\"",
                        state.status.as_str()
                    ));
                }
                settle_wall_clock(&mut state, now);
                state.status = GoalStatus::Active;
                state.terminal_reason = None;
                state.wall_clock_resumed_at = Some(now);
                state.updated_at = now;
                if let Some(outcome) = self.block_if_budget_reached(&mut state, now) {
                    return Ok(outcome);
                }
            }
            "paused" => {
                // v2 `pauseGoal`: already paused is idempotent (and keeps the
                // recorded reason); any other non-active status is invalid.
                if state.status == GoalStatus::Paused {
                    return Ok(goal_outcome(&state, now));
                }
                if state.status != GoalStatus::Active {
                    return Err(format!(
                        "State write error: [-32004] Cannot pause a goal in status \"{}\"",
                        state.status.as_str()
                    ));
                }
                settle_wall_clock(&mut state, now);
                state.status = GoalStatus::Paused;
                state.terminal_reason = reason.map(str::to_string);
                state.wall_clock_resumed_at = None;
            }
            "complete" => {
                if state.status != GoalStatus::Active {
                    return Err(
                        "State write error: [-32004] Goal not completed: no active goal.".into(),
                    );
                }
                settle_wall_clock(&mut state, now);
                state.status = GoalStatus::Complete;
                state.terminal_reason = reason.map(str::to_string);
                state.wall_clock_resumed_at = None;
            }
            _ => {
                if state.status != GoalStatus::Active {
                    return Err(
                        "State write error: [-32004] Goal not blocked: no active goal.".into(),
                    );
                }
                settle_wall_clock(&mut state, now);
                state.status = GoalStatus::Blocked;
                state.terminal_reason = reason.map(str::to_string);
                state.wall_clock_resumed_at = None;
            }
        }
        state.updated_at = now;
        Ok(goal_outcome(&state, now))
    }

    fn goal_set_budget(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(budget_value) = value.get("value").and_then(|v| v.as_f64()) else {
            return Err(
                "State write error: [-32003] invalid goal set_budget value: expected { action: \"set_budget\", value: number, unit: string }".into(),
            );
        };
        let Some(unit_str) = value.get("unit").and_then(|u| u.as_str()) else {
            return Err(
                "State write error: [-32003] invalid goal set_budget value: expected { action: \"set_budget\", value: number, unit: string }".into(),
            );
        };
        let Some(unit) = crate::goal::BudgetUnit::parse_unit(unit_str) else {
            return Err(
                "State write error: [-32003] invalid goal set_budget value: unit must be one of turns, tokens, milliseconds, seconds, minutes, hours".into(),
            );
        };
        let Some(limits) = crate::goal::budget_limits_from_input(budget_value, unit) else {
            return Err(format!(
                "State write error: [-32003] Goal budget not set: {} is not a reasonable goal budget.",
                crate::goal::format_budget(budget_value, unit)
            ));
        };
        let Some(mut state) = self.read_goal_state()? else {
            return Err("State write error: [-32004] No current goal".into());
        };
        // v2 merges the new limit into the existing limits.
        if limits.token_budget.is_some() {
            state.budget_limits.token_budget = limits.token_budget;
        }
        if limits.turn_budget.is_some() {
            state.budget_limits.turn_budget = limits.turn_budget;
        }
        if limits.wall_clock_budget_ms.is_some() {
            state.budget_limits.wall_clock_budget_ms = limits.wall_clock_budget_ms;
        }
        let now = now_ms();
        state.updated_at = now;
        // v2 `setBudgetLimits` runs `blockIfBudgetReached` after the merge: a
        // limit that is already spent blocks the goal in the same write.
        if let Some(outcome) = self.block_if_budget_reached(&mut state, now) {
            return Ok(outcome);
        }
        Ok(goal_outcome(&state, now))
    }

    fn apply_cron_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(action) = value.get("action").and_then(|a| a.as_str()) else {
            return Err(
                "State write error: [-32003] invalid cron state value: expected { action: \"create\" | \"delete\", ... }".into(),
            );
        };
        match action {
            "create" => self.cron_create(value),
            "delete" => self.cron_delete(value),
            _ => Err(format!(
                "State write error: [-32003] invalid cron action: {action}"
            )),
        }
    }

    fn cron_create(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(cron) = value.get("cron").and_then(|c| c.as_str()) else {
            return Err(
                "State write error: [-32003] invalid cron create value: expected { action: \"create\", cron: string, prompt: string, recurring?: boolean }".into(),
            );
        };
        let Some(prompt) = value.get("prompt").and_then(|p| p.as_str()) else {
            return Err(
                "State write error: [-32003] invalid cron create value: expected { action: \"create\", cron: string, prompt: string, recurring?: boolean }".into(),
            );
        };
        let recurring = value
            .get("recurring")
            .and_then(|r| r.as_bool())
            .unwrap_or(true);
        let mut entries = self.read_domain_list("cron")?;
        // No scheduler in the REPL yet, so `nextFireAt` stays null (the
        // renderers show "null") and `stale` is always false.
        let entry = json!({
            "id": ulid(),
            "cron": cron,
            "prompt": prompt,
            "createdAt": now_ms(),
            "recurring": recurring,
            "nextFireAt": null,
            "stale": false,
        });
        entries.push(entry.clone());
        Ok(StateWriteOutcome {
            stored: Value::Array(entries),
            response: entry,
        })
    }

    fn cron_delete(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(id) = value.get("id").and_then(|i| i.as_str()) else {
            return Err(
                "State write error: [-32003] invalid cron delete value: expected { action: \"delete\", id: string }".into(),
            );
        };
        let mut entries = self.read_domain_list("cron")?;
        let before = entries.len();
        entries.retain(|entry| entry.get("id").and_then(|i| i.as_str()) != Some(id));
        if entries.len() == before {
            return Err(format!(
                "State write error: [-32004] No cron job with id {id}."
            ));
        }
        let stored = Value::Array(entries);
        Ok(StateWriteOutcome {
            stored: stored.clone(),
            response: stored,
        })
    }

    fn apply_task_write(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(action) = value.get("action").and_then(|a| a.as_str()) else {
            return Err(
                "State write error: [-32003] invalid task state value: expected { action: \"stop\" | \"wait\", ... }".into(),
            );
        };
        match action {
            "stop" => self.task_stop(value),
            "wait" => self.task_wait(value),
            _ => Err(format!(
                "State write error: [-32003] invalid task action: {action}"
            )),
        }
    }

    fn task_stop(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(id) = value.get("id").and_then(|i| i.as_str()) else {
            return Err(
                "State write error: [-32003] invalid task stop value: expected { action: \"stop\", id: string }".into(),
            );
        };
        let mut tasks = self.read_domain_list("task")?;
        let Some(index) = tasks
            .iter()
            .position(|t| t.get("taskId").and_then(|i| i.as_str()) == Some(id))
        else {
            return Err(format!("State write error: [-32002] Task not found: {id}"));
        };
        let mut entry = tasks[index].clone();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("status".into(), json!("killed"));
            obj.insert("stopReason".into(), json!("Stopped by TaskStop"));
            obj.insert("endedAt".into(), json!(now_ms()));
        }
        tasks[index] = entry.clone();
        Ok(StateWriteOutcome {
            stored: Value::Array(tasks),
            response: entry,
        })
    }

    fn task_wait(&self, value: &Value) -> Result<StateWriteOutcome, String> {
        let Some(id) = value.get("id").and_then(|i| i.as_str()) else {
            return Err(
                "State write error: [-32003] invalid task wait value: expected { action: \"wait\", id: string, timeout_ms?: number }".into(),
            );
        };
        let tasks = self.read_domain_list("task")?;
        let Some(entry) = tasks
            .iter()
            .find(|t| t.get("taskId").and_then(|i| i.as_str()) == Some(id))
            .cloned()
        else {
            return Err(format!("State write error: [-32002] Task not found: {id}"));
        };
        // The REPL has no background task runner yet: a terminal task
        // reports completion, anything else reports the v2 wait outcome for
        // a still-running task (an immediate timeout report).
        Ok(StateWriteOutcome {
            stored: Value::Array(tasks),
            response: entry,
        })
    }

    /// Adopt the host's live goal snapshot into the local store.
    ///
    /// On the native host the user-facing lifecycle (`createGoal` /
    /// `pauseGoal` / `resumeGoal` / `cancelGoal`) is host-initiated, but every
    /// engine side effect — the turn-end fold, the model's `UpdateGoal` /
    /// `SetGoalBudget` — has to land in one store. A goal the store does not
    /// have yet, or one whose id differs from the host's (a replace), is built
    /// from the host snapshot. Otherwise only a snapshot *newer* than the
    /// store's last write is applied: that is a host-side status transition
    /// the engine has not seen yet. Counters and budgets stay engine-owned, so
    /// a host snapshot that lags the last fold can never roll them back.
    pub fn sync_goal_from_context(
        &self,
        goal: &crate::rpc::types::GoalContext,
    ) -> Result<bool, String> {
        // Adopting a host snapshot is a read-modify-write of the goal: the
        // decision to adopt (and what to adopt) comes from the stored state.
        // The lock spans both so a concurrent fold or model write cannot be
        // rolled back by a snapshot computed from the older value.
        let lock = self.domain_lock("goal");
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        let Some(mut state) = self
            .read_goal_state()?
            .filter(|state| state.goal_id == goal.goal_id)
        else {
            let state = goal_state_from_context(goal, now);
            self.write_domain_locked("goal", &goal_outcome(&state, now).stored)?;
            return Ok(true);
        };
        // The host knows this goal now (it created it, or adopted the engine's
        // `goal.updated`), so a later "no goal" from the host may clear it.
        let adopted = !state.host_owned;
        state.host_owned = true;
        if goal.updated_at <= state.updated_at as i64 {
            if adopted {
                self.write_domain_locked("goal", &goal_outcome(&state, now).stored)?;
            }
            return Ok(adopted);
        }
        let status = host_goal_status(goal.status);
        if status == state.status {
            if adopted {
                self.write_domain_locked("goal", &goal_outcome(&state, now).stored)?;
            }
            return Ok(adopted);
        }
        settle_wall_clock(&mut state, now);
        state.status = status;
        // A goal that is no longer active carries the reason it stopped; a
        // host that names none leaves the field empty rather than invented.
        state.terminal_reason = if status == GoalStatus::Active {
            None
        } else {
            goal.terminal_reason.clone()
        };
        state.wall_clock_resumed_at = if status == GoalStatus::Active {
            Some(now)
        } else {
            None
        };
        state.updated_at = now;
        self.write_domain_locked("goal", &goal_outcome(&state, now).stored)?;
        Ok(true)
    }

    /// Drop the stored goal when the host reports it cleared its own copy
    /// (v2 `cancelGoal`). Only a goal the host has reported back to the
    /// engine can be cleared this way: an engine-created goal that the host
    /// has not adopted yet would otherwise be lost to a stale "no goal".
    /// `false` when there was nothing to clear.
    pub fn clear_host_goal(&self) -> Result<bool, String> {
        let lock = self.domain_lock("goal");
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = self.read_goal_state()? else {
            return Ok(false);
        };
        if !state.host_owned {
            return Ok(false);
        }
        self.write_domain_locked("goal", &json!({ "goal": Value::Null }))?;
        Ok(true)
    }

    /// The stored goal projected as the model/host-facing snapshot, with the
    /// live wall-clock figure folded in. `None` when no goal is stored.
    pub fn goal_snapshot(&self) -> Option<Value> {
        // The host status read has no error channel yet (the goal path gets one
        // together with the turn-loop hook); a failed read is logged and
        // reported as "no goal" rather than invented from an empty file.
        let state = match self.read_goal_state() {
            Ok(Some(state)) => state,
            Ok(None) => return None,
            Err(cause) => {
                tracing::warn!(%cause, "goal state is unreadable; reporting no goal");
                return None;
            }
        };
        serde_json::to_value(state.snapshot_now()).ok()
    }

    /// The stored goal, `Ok(None)` when no goal is stored.
    ///
    /// A goal file that cannot be read or decoded is an error: every caller
    /// here decides whether to create, update or clear the goal, and "no goal"
    /// is a destructive answer (it would authorise a create that overwrites the
    /// stored one).
    fn read_goal_state(&self) -> Result<Option<GoalState>, String> {
        let stored = match self.read_domain_state("goal") {
            ReadOutcome::Found(value) => value,
            ReadOutcome::Absent => return Ok(None),
            ReadOutcome::Unreadable(cause) => {
                return Err(format!("goal state is unreadable: {cause}"));
            }
        };
        let Some(goal) = stored.get("goal").cloned() else {
            return Ok(None);
        };
        if goal.is_null() {
            return Ok(None);
        }
        serde_json::from_value(goal)
            .map(Some)
            .map_err(|error| format!("goal state is malformed: {error}"))
    }

    /// The task output log path (`<state_dir>/tasks/<task_id>/output.log`),
    /// mirroring the v2 `tasks/<taskId>/output.log` layout.
    pub fn task_output_path(&self, task_id: &str) -> PathBuf {
        self.state_dir
            .join("tasks")
            .join(task_id)
            .join("output.log")
    }

    /// Persist a task's output log (v2 `writeTaskOutputData` semantics:
    /// the output is written to disk so `TaskOutput` survives a restart).
    /// Best-effort: a failed write only logs.
    ///
    /// The write is capped at [`MAX_PERSISTED_TASK_OUTPUT_BYTES`]. v2 never
    /// lets a task's log grow without bound either — its in-process producer
    /// stops at 16 MiB (`MAX_TASK_OUTPUT_BYTES`,
    /// `agent/task/taskService.ts:151`) — but v2 arrives there by *streaming*,
    /// while this store is handed one finished `String`. Without a cap here a
    /// single task could write an arbitrary amount to disk, which is the
    /// unbounded-growth half of the same defect. The tail is kept, because
    /// that is what answers "what happened" and what v2's own preview keeps.
    pub fn write_task_output(&self, task_id: &str, output: &str) {
        let path = self.task_output_path(task_id);
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            tracing::warn!(
                task_id,
                dir = %parent.display(),
                "task output was not persisted: could not create the output directory: {e}"
            );
            return;
        }
        // Atomic, like every other state write: a reader must never see a
        // half-written log, and a crash mid-write must not truncate the
        // previous one (the old `fs::write` could do both).
        if let Err(error) = atomic_replace(&path, cap_output_tail(output).as_bytes()) {
            tracing::warn!(task_id, "task output was not persisted: {error}");
        }
    }

    /// Read a task's output snapshot from its log file, mirroring the v2
    /// `readTaskOutputSnapshot` shape (`outputPath` / `outputSizeBytes` /
    /// `previewBytes` / `truncated` / `preview`). `None` when no log
    /// exists.
    pub fn read_task_output(&self, task_id: &str, max_preview_bytes: usize) -> Option<Value> {
        let path = self.task_output_path(task_id);
        let data = match fs::read(&path) {
            Ok(data) => data,
            // No log yet is the ordinary case for a task that never wrote one.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            // Any other failure is not "no output": reporting it as absent
            // would tell the model the task produced nothing.
            Err(error) => {
                tracing::warn!(
                    task_id,
                    path = %path.display(),
                    "task output exists but could not be read: {error}"
                );
                return None;
            }
        };
        let preview_limit = max_preview_bytes.min(data.len());
        let preview_offset = data.len() - preview_limit;
        let preview = String::from_utf8_lossy(&data[preview_offset..]).into_owned();
        Some(json!({
            "outputPath": path.to_string_lossy(),
            "outputSizeBytes": data.len(),
            "previewBytes": preview_limit,
            "truncated": preview_offset > 0,
            "fullOutputAvailable": true,
            "preview": preview,
        }))
    }

    /// Pause a goal that was active when the engine last stopped, without
    /// charging the offline time (v2 `resumePersisted` →
    /// `pauseGoal(goal, 'Paused after agent resume')`,
    /// `test/wire/resume.test.ts:850-911`).
    ///
    /// The wall clock an active goal shows is `settled + (now - resumed_at)`.
    /// After a restart `resumed_at` is from the previous process, so reading it
    /// would charge every minute the engine was not running — a goal with a
    /// wall-clock budget could come back already over budget because the user
    /// went home. v2 settles at the stored figure, pauses, and clears the
    /// anchor; resuming is an explicit user action.
    ///
    /// Returns the paused snapshot when it had something to do.
    pub fn pause_goal_on_resume(&self) -> Option<crate::goal::GoalSnapshot> {
        // Resume-time housekeeping runs before the first turn, where the
        // caller has no error channel yet; a failed read is logged and treated
        // as "nothing to pause" rather than pausing over a file it could not
        // read.
        let lock = self.domain_lock("goal");
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = match self.read_goal_state() {
            Ok(Some(state)) => state,
            Ok(None) => return None,
            Err(cause) => {
                tracing::warn!(%cause, "goal state is unreadable; not pausing on resume");
                return None;
            }
        };
        if state.status != GoalStatus::Active {
            return None;
        }
        let now = now_ms();
        // Deliberately not `live_wall_clock_ms`: the gap since the last
        // resume is offline time, and v2 never charges it.
        state.wall_clock_resumed_at = None;
        state.status = GoalStatus::Paused;
        state.updated_at = now;
        let outcome = goal_outcome(&state, now);
        if let Err(error) = self.write_domain_locked("goal", &outcome.stored) {
            tracing::warn!(%error, "could not persist the goal paused on resume");
            return None;
        }
        Some(to_snapshot(&state, state.wall_clock_ms))
    }

    /// The stored goal projected into the turn-loop `GoalContext`, with the
    /// live wall-clock figure folded in for active goals. `None` when no
    /// goal is stored.
    pub fn goal_context(&self) -> Option<crate::turn_loop::types::GoalContext> {
        // The turn-loop hook has no error channel yet; a failed read is logged
        // and the hook sees "no goal this turn" instead of an invented one.
        let state = match self.read_goal_state() {
            Ok(Some(state)) => state,
            Ok(None) => return None,
            Err(cause) => {
                tracing::warn!(%cause, "goal state is unreadable; the turn sees no goal");
                return None;
            }
        };
        let now = now_ms();
        let status = match state.status {
            GoalStatus::Active => crate::turn_loop::types::GoalStatus::Active,
            GoalStatus::Paused => crate::turn_loop::types::GoalStatus::Paused,
            GoalStatus::Blocked => crate::turn_loop::types::GoalStatus::Blocked,
            GoalStatus::Complete => crate::turn_loop::types::GoalStatus::Complete,
            GoalStatus::BudgetLimited => crate::turn_loop::types::GoalStatus::BudgetLimited,
            GoalStatus::UsageLimited => crate::turn_loop::types::GoalStatus::UsageLimited,
        };
        let wall_clock_ms = live_wall_clock_ms(&state, now) as i64;
        Some(crate::turn_loop::types::GoalContext {
            goal_id: state.goal_id,
            objective: state.objective,
            status,
            token_budget: state.budget_limits.token_budget.map(|v| v as i64),
            turn_budget: state.budget_limits.turn_budget.map(|v| v as i64),
            wall_clock_budget_ms: state.budget_limits.wall_clock_budget_ms.map(|v| v as i64),
            wall_clock_ms,
            tokens_used: state.tokens_used as i64,
            turns_used: state.turns_used as i64,
            updated_at: state.updated_at as i64,
            terminal_reason: state.terminal_reason.clone(),
        })
    }
}

/// Map the host-wire goal status onto the persisted spelling.
fn host_goal_status(status: crate::rpc::types::GoalStatus) -> GoalStatus {
    use crate::rpc::types::GoalStatus as HostGoalStatus;
    match status {
        HostGoalStatus::Active => GoalStatus::Active,
        HostGoalStatus::Paused => GoalStatus::Paused,
        HostGoalStatus::Blocked => GoalStatus::Blocked,
        HostGoalStatus::Complete => GoalStatus::Complete,
        HostGoalStatus::BudgetLimited => GoalStatus::BudgetLimited,
        HostGoalStatus::UsageLimited => GoalStatus::UsageLimited,
    }
}

/// Build the local goal record from a host snapshot (first adoption of a
/// host-created goal, or a replace onto a new goal id).
fn goal_state_from_context(goal: &crate::rpc::types::GoalContext, now: u64) -> GoalState {
    let status = host_goal_status(goal.status);
    let tokens_used = u64::try_from(goal.tokens_used.max(0)).unwrap_or(0);
    let wall_clock_ms = u64::try_from(goal.wall_clock_ms.max(0)).unwrap_or(0);
    let budget_of = |value: Option<i64>| {
        value
            .and_then(|value| u64::try_from(value).ok())
            .filter(|value| *value > 0)
    };
    GoalState {
        goal_id: goal.goal_id.clone(),
        objective: goal.objective.clone(),
        completion_criterion: None,
        status,
        turns_used: u64::try_from(goal.turns_used.max(0)).unwrap_or(0),
        tokens_used,
        input_tokens_used: 0,
        output_tokens_used: tokens_used,
        wall_clock_ms,
        wall_clock_resumed_at: (status == GoalStatus::Active).then_some(now),
        budget_limits: GoalBudgetLimits {
            token_budget: budget_of(goal.token_budget),
            turn_budget: budget_of(goal.turn_budget),
            wall_clock_budget_ms: budget_of(goal.wall_clock_budget_ms),
        },
        terminal_reason: None,
        host_owned: true,
        created_at: now.saturating_sub(wall_clock_ms),
        updated_at: now,
    }
}

/// The stored goal domain value `{goal: <GoalState> | null}` and the
/// snapshot response the caller renders from it.
///
/// The **state** is what is persisted: it carries `wallClockResumedAt` and
/// `budgetLimits`, without which the live wall-clock figure would reset to
/// the last write on every read. The **snapshot** is what a caller (the tool
/// output, the host event) renders — v2 keeps the same split between its
/// durable `GoalState` and its `GoalSnapshot`.
fn goal_outcome(state: &GoalState, now_ms: u64) -> StateWriteOutcome {
    let snapshot = to_snapshot(state, live_wall_clock_ms(state, now_ms));
    let stored = json!({ "goal": serde_json::to_value(state).unwrap_or(Value::Null) });
    StateWriteOutcome {
        stored,
        response: json!({ "goal": serde_json::to_value(&snapshot).unwrap_or(Value::Null) }),
    }
}

/// The live wall-clock figure for an active goal (v2 `toSnapshot`
/// semantics: stored elapsed time plus time since the last resume).
fn live_wall_clock_ms(state: &GoalState, now_ms: u64) -> u64 {
    state.live_wall_clock_ms(now_ms)
}

/// Fold the elapsed time since the last resume into `wall_clock_ms` and
/// clear the resume anchor (v2 `settleWallClock` on lifecycle transitions).
fn settle_wall_clock(state: &mut GoalState, now_ms: u64) {
    state.wall_clock_ms = live_wall_clock_ms(state, now_ms);
    state.wall_clock_resumed_at = None;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A v2-style 26-character Crockford base32 ULID (48-bit timestamp + 80
/// bits of randomness), matching the id shape the cron tools validate.
fn ulid() -> String {
    const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut out = String::with_capacity(26);
    let ts = now_ms() & 0xFFFF_FFFF_FFFF;
    for shift in (0..48).step_by(5).rev() {
        out.push(CROCKFORD[((ts >> shift) & 0x1F) as usize] as char);
    }
    for _ in 0..16 {
        out.push(CROCKFORD[(fastrand::u32(..) & 0x1F) as usize] as char);
    }
    out
}

impl crate::injection::DomainValueSource for StateStore {
    fn read_domain_state(&self, domain: &str) -> ReadOutcome<serde_json::Value> {
        StateStore::read_domain_state(self, domain)
    }
}

/// The checkpoint file name, zero-padded for lexicographic sort.
fn checkpoint_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("{seq:08}.json"))
}

/// The next checkpoint sequence number: one past the highest existing
/// file. Scans the directory; for the REPL's checkpoint cadence (one per
/// turn) the directory stays small and the scan is negligible.
fn next_checkpoint_seq(dir: &Path) -> Result<u64, String> {
    let mut max: Option<u64> = None;
    for entry in fs::read_dir(dir).map_err(|e| format!("read checkpoints dir: {e}"))? {
        let entry = entry.map_err(|e| format!("checkpoint dir entry: {e}"))?;
        if let Some(seq) = parse_seq(&entry.file_name().to_string_lossy()) {
            max = Some(max.map_or(seq, |m| m.max(seq)));
        }
    }
    Ok(max.map_or(1, |m| m + 1))
}

/// Read one domain's stored value for a workspace, as the JSON the store holds.
///
/// The store's directory name is a digest of the canonicalized workspace path,
/// so a caller outside this crate cannot locate the file itself. The napi
/// export and the host both go through here rather than re-deriving the path —
/// the host used to guess `<sessionDir>/todo.json`, which nothing ever writes.
/// Read a domain with the v2 scope rule: `cron` is workspace-level, every
/// other engine domain is session-level. A missing session store falls back to
/// the workspace store, which is also how a pre-scope install keeps its data
/// readable after this change.
pub fn read_scoped_state(
    workspace_root: &Path,
    session_id: Option<&str>,
    domain: &str,
) -> Result<Option<String>, String> {
    let encode = |value: Option<Value>| -> Result<Option<String>, String> {
        match value {
            None => Ok(None),
            Some(value) => serde_json::to_string(&value)
                .map(Some)
                .map_err(|error| format!("cannot encode {domain} state: {error}")),
        }
    };
    let workspace = || -> Result<Option<Value>, String> {
        let store = StateStore::for_workspace(workspace_root)
            .map_err(|error| format!("cannot locate the workspace state store: {error}"))?;
        store
            .read_domain_state(domain)
            .into_result()
            .map_err(|cause| cause.to_string())
    };
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return encode(workspace()?);
    };
    if domain == "cron" {
        return encode(workspace()?);
    }
    // A session with no store yet falls through to the workspace store; a
    // session store that exists but cannot be read is an error.
    let session = match StateStore::for_session(workspace_root, session_id) {
        Ok(store) => store
            .read_domain_state(domain)
            .into_result()
            .map_err(|cause| cause.to_string())?,
        Err(_) => None,
    };
    encode(match session {
        Some(value) => Some(value),
        None => workspace()?,
    })
}

/// Pop the newest `count` session-state checkpoints (native undo).
///
/// Undo restores the state of the turns it cuts, so the caller pops one
/// snapshot per removed turn. The stack is LIFO; an exhausted stack stops the
/// loop quietly — a session that never checkpointed (or one whose history was
/// rebuilt by a fork) is normal, not a failure. Returns how many snapshots
/// were consumed.
pub fn rollback_scoped_state(workspace_root: &Path, session_id: &str, count: usize) -> usize {
    if count == 0 || session_id.is_empty() {
        return 0;
    }
    let Ok(store) = StateStore::for_session(workspace_root, session_id) else {
        return 0;
    };
    let mut rolled_back = 0;
    for _ in 0..count {
        match store.rollback() {
            Ok(true) => rolled_back += 1,
            Ok(false) => break,
            Err(error) => {
                tracing::warn!(%error, "state rollback stopped early");
                break;
            }
        }
    }
    rolled_back
}

pub fn read_workspace_state(workspace_root: &Path, domain: &str) -> Result<Option<String>, String> {
    let store = StateStore::for_workspace(workspace_root)
        .map_err(|error| format!("cannot locate the workspace state store: {error}"))?;
    let Some(value) = store
        .read_domain_state(domain)
        .into_result()
        .map_err(|cause| cause.to_string())?
    else {
        return Ok(None);
    };
    serde_json::to_string(&value)
        .map(Some)
        .map_err(|error| format!("cannot encode {domain} state: {error}"))
}

fn latest_checkpoint_seq(dir: &Path) -> Result<Option<u64>, String> {
    let mut max: Option<u64> = None;
    for entry in fs::read_dir(dir).map_err(|e| format!("read checkpoints dir: {e}"))? {
        let entry = entry.map_err(|e| format!("checkpoint dir entry: {e}"))?;
        if let Some(seq) = parse_seq(&entry.file_name().to_string_lossy()) {
            max = Some(max.map_or(seq, |m| m.max(seq)));
        }
    }
    Ok(max)
}

fn count_checkpoint_files(dir: &Path) -> Result<usize, String> {
    let mut n = 0usize;
    for entry in fs::read_dir(dir).map_err(|e| format!("read checkpoints dir: {e}"))? {
        let entry = entry.map_err(|e| format!("checkpoint dir entry: {e}"))?;
        if parse_seq(&entry.file_name().to_string_lossy()).is_some() {
            n += 1;
        }
    }
    Ok(n)
}

/// Delete all but the newest `max` snapshots, returning how many were removed.
///
/// The oldest entries are the ones undo can no longer reach, so they are the
/// ones dropped. A file that cannot be removed is an error the caller reports:
/// silently leaving it behind is how the stack looks bounded while the disk
/// keeps growing.
fn prune_checkpoint_files(dir: &Path, max: usize) -> Result<usize, String> {
    let mut seqs: Vec<u64> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("read checkpoints dir: {e}"))? {
        let entry = entry.map_err(|e| format!("checkpoint dir entry: {e}"))?;
        if let Some(seq) = parse_seq(&entry.file_name().to_string_lossy()) {
            seqs.push(seq);
        }
    }
    if seqs.len() <= max {
        return Ok(0);
    }
    seqs.sort_unstable();
    let remove_upto = seqs.len() - max;
    let mut removed = 0usize;
    for seq in &seqs[..remove_upto] {
        fs::remove_file(checkpoint_path(dir, *seq))
            .map_err(|e| format!("checkpoint remove seq {seq}: {e}"))?;
        removed += 1;
    }
    Ok(removed)
}

fn parse_seq(name: &str) -> Option<u64> {
    name.strip_suffix(".json").and_then(|s| s.parse().ok())
}

fn write_checkpoint_file(dir: &Path, seq: u64, snapshot: &StateSnapshot) -> Result<(), String> {
    let path = checkpoint_path(dir, seq);
    let json = serde_json::to_string(snapshot).map_err(|e| format!("serialize checkpoint: {e}"))?;
    atomic_replace(&path, json.as_bytes())
}

/// Replace `path` with `bytes` atomically: a temp file in the same directory
/// under a name unique to this call, fsynced, renamed over the target, then
/// the directory entry fsynced.
fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = unique_tmp_path(path);
    let mut file =
        fs::File::create(&tmp).map_err(|e| format!("create tmp file {}: {e}", tmp.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write tmp file {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename tmp file: {e}"))?;
    fsync_dir(path.parent());
    Ok(())
}

/// A temp path no other writer can be holding: the fixed `<domain>.json.tmp`
/// name let two in-process writers truncate each other's staging file. It
/// must stay in the target's own directory — `rename` is only atomic within
/// one filesystem.
fn unique_tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(
        "{name}.tmp.{}.{}",
        std::process::id(),
        fastrand::u32(..)
    ))
}

/// fsync the directory holding the freshly renamed file. The file's own bytes
/// are already fsynced, but on POSIX the rename is a separate directory
/// update that a power loss can drop, leaving the old or no file behind.
/// Best-effort: the data is written either way, and Windows journals the
/// rename (a directory handle needs `FILE_FLAG_BACKUP_SEMANTICS`, which the
/// std `File::open` does not offer), so there it is a no-op.
fn fsync_dir(dir: Option<&Path>) {
    #[cfg(unix)]
    if let Some(dir) = dir
        && let Err(e) = fs::File::open(dir).and_then(|handle| handle.sync_all())
    {
        tracing::warn!(dir = %dir.display(), %e, "could not fsync the state directory after a rename");
    }
    #[cfg(not(unix))]
    let _ = dir;
}
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, StateStore) {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
        (tmp, store)
    }

    /// The read side a host reaches through the napi export: it resolves the
    /// workspace-keyed directory itself, so the caller never has to know the
    /// derivation. A domain with no stored state reads as `None` rather than
    /// an error, and an unknown domain is `None` too.
    #[test]
    fn read_workspace_state_resolves_the_workspace_directory() {
        let workspace = TempDir::new().unwrap();
        let todos = json!([{ "title": "Read session-control.ts", "status": "in_progress" }]);
        StateStore::for_workspace(workspace.path())
            .unwrap()
            .write_domain("todo", &todos)
            .unwrap();

        let raw = read_workspace_state(workspace.path(), "todo")
            .expect("the state store must be readable")
            .expect("the todo domain is set");
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), todos);

        assert!(
            read_workspace_state(workspace.path(), "plan")
                .unwrap()
                .is_none()
        );
        assert!(
            read_workspace_state(workspace.path(), "not-a-domain")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_round_trip_all_domains() {
        let (_tmp, store) = store();
        let cases = [
            (
                "todo",
                json!([
                    { "id": "T1", "parentId": null, "kind": "task", "title": "Read", "status": "in_progress", "progress": 40 }
                ]),
            ),
            (
                "plan",
                json!({ "active": true, "id": "plan-1", "path": "/plans/plan-1.md" }),
            ),
            (
                "goal",
                json!({ "goal": { "goalId": "goal-1", "objective": "Ship", "status": "active", "turnsUsed": 0, "tokensUsed": 0, "inputTokensUsed": 0, "outputTokensUsed": 0, "wallClockMs": 0, "budget": { "tokenBudget": null, "turnBudget": null, "wallClockBudgetMs": null, "remainingTokens": null, "remainingTurns": null, "remainingWallClockMs": null, "tokenBudgetReached": false, "turnBudgetReached": false, "wallClockBudgetReached": false, "overBudget": false, "inputTokensUsed": 0, "outputTokensUsed": 0 }, "createdAt": 1, "updatedAt": 1 } }),
            ),
            (
                "cron",
                json!([{ "id": "01ABCDEFGHJKMNPQRSTVWXYZ", "cron": "0 9 * * *", "prompt": "daily", "createdAt": 1, "recurring": true, "nextFireAt": null, "stale": false }]),
            ),
            (
                "task",
                json!([{ "taskId": "task-1", "description": "run", "status": "running", "startedAt": 1 }]),
            ),
        ];
        for (domain, value) in cases {
            store.write_domain(domain, &value).unwrap();
            assert_eq!(
                store.read_domain_state(domain).into_result().unwrap(),
                Some(value),
                "domain: {domain}"
            );
        }
    }

    #[test]
    fn test_atomic_write_replaces_and_leaves_no_tmp() {
        let (_tmp, store) = store();
        store
            .write_domain(
                "todo",
                &json!([{ "id": "T1", "title": "First", "status": "pending" }]),
            )
            .unwrap();
        store
            .write_domain(
                "todo",
                &json!([{ "id": "T2", "title": "Second", "status": "done" }]),
            )
            .unwrap();
        let value = store
            .read_domain_state("todo")
            .into_result()
            .unwrap()
            .unwrap();
        assert_eq!(value[0]["id"], "T2");
        assert_eq!(value[0]["title"], "Second");
        let leftovers: Vec<_> = fs::read_dir(store.state_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");
    }

    #[test]
    fn test_defaults_for_empty_domains() {
        let (_tmp, store) = store();
        for (domain, expected) in [
            ("todo", json!([])),
            ("plan", json!({ "active": false })),
            ("goal", json!({ "goal": null })),
            ("cron", json!([])),
            ("task", json!([])),
        ] {
            assert_eq!(
                store.read_state(domain, domain).unwrap(),
                expected,
                "domain: {domain}"
            );
        }
    }

    /// A domain file that exists but cannot be read or parsed is corrupt, not
    /// absent: the three on-disk states must stay distinguishable, because
    /// the other two tests below depend on it.
    #[test]
    fn test_read_domain_state_distinguishes_absent_corrupt_and_valid() {
        let (_tmp, store) = store();

        // Absent: no file yet (and an unknown domain name).
        assert!(matches!(
            store.read_domain_state("todo"),
            DomainRead::Absent
        ));
        assert!(matches!(
            store.read_domain_state("not-a-domain"),
            DomainRead::Absent
        ));
        assert_eq!(store.read_domain_state("todo").into_result().unwrap(), None);

        // Corrupt: a truncated file (a crash or a shared volume cutting a
        // write short) must not read as "no todos".
        let todo_path = store.state_dir.join("todo.json");
        fs::write(&todo_path, "[{\"id\":\"T1\"").unwrap();
        let read = store.read_domain_state("todo");
        assert!(
            read.is_unreadable(),
            "a truncated file must read as unreadable"
        );
        assert!(
            read.cause()
                .is_some_and(|cause| cause.subject.contains("todo.json")),
            "the cause names the file: {:?}",
            read.cause()
        );
        // The three states stay separable: an unreadable domain must not be
        // reported as "no todos".
        assert!(store.read_domain_state("todo").into_result().is_err());

        // Valid: the value round-trips and the file is no longer corrupt.
        let todos = json!([{ "id": "T1", "title": "Read" }]);
        store.write_domain("todo", &todos).unwrap();
        match store.read_domain_state("todo") {
            ReadOutcome::Found(value) => assert_eq!(value, todos),
            other => panic!("a reparsed file must read as a value, got {other:?}"),
        }
        assert!(!store.read_domain_state("todo").is_unreadable());
    }

    /// A corrupt domain must never enter a snapshot as `None`, because
    /// `rollback` reads `None` as "delete this domain's file".
    #[test]
    fn test_checkpoint_refuses_to_freeze_a_corrupt_domain() {
        let (_tmp, store) = store();
        store
            .write_domain("plan", &json!({ "active": true, "id": "plan-1" }))
            .unwrap();
        store.checkpoint().unwrap();
        assert_eq!(store.checkpoint_depth(), 1);

        // The plan file is truncated by a crash.
        fs::write(store.state_dir.join("plan.json"), "{\"active\": tr").unwrap();
        let error = store.checkpoint().unwrap_err();
        assert!(
            error.contains("plan") && error.contains("refusing to checkpoint"),
            "checkpoint must name the domain and refuse: {error}"
        );
        assert_eq!(
            store.checkpoint_depth(),
            1,
            "the refused checkpoint must not push a stack entry"
        );

        // An absent domain is not corrupt: checkpointing still works.
        assert!(matches!(
            store.read_domain_state("todo"),
            DomainRead::Absent
        ));
        store.clear_domain("plan").unwrap();
        store.checkpoint().unwrap();
        assert_eq!(store.checkpoint_depth(), 2);
    }

    /// Undo must never delete a domain file it could not read. The snapshot
    /// says the domain was absent; the bytes on disk are the only copy.
    #[test]
    fn test_rollback_never_deletes_a_corrupt_domain() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        {
            let store = StateStore::for_dir(state_dir.clone()).unwrap();
            store.checkpoint().unwrap();
            assert_eq!(store.checkpoint_depth(), 1);
        }
        // The todo file is truncated after the snapshot was taken.
        let todo_path = state_dir.join("todo.json");
        fs::write(&todo_path, "[{\"id\":\"T1\",\"title\":").unwrap();

        let store = StateStore::for_dir(state_dir).unwrap();
        assert!(store.rollback().unwrap());
        assert!(
            todo_path.is_file(),
            "rollback deleted a domain file it could not read"
        );
        assert_eq!(
            fs::read_to_string(&todo_path).unwrap(),
            "[{\"id\":\"T1\",\"title\":",
            "the corrupt bytes must survive untouched"
        );
        assert!(store.read_domain_state("todo").is_unreadable());
        assert_eq!(store.checkpoint_depth(), 0, "the snapshot is consumed");

        // A genuinely absent domain is still cleared by a snapshot recorded
        // the same way — the corrupt file is not what makes rollback skip.
        let fresh = TempDir::new().unwrap();
        let fresh_dir = fresh.path().join("state");
        let store = StateStore::for_dir(fresh_dir.clone()).unwrap();
        store.checkpoint().unwrap();
        store
            .write_domain("plan", &json!({ "active": true, "id": "plan-1" }))
            .unwrap();
        assert!(store.rollback().unwrap());
        assert!(matches!(
            store.read_domain_state("plan"),
            DomainRead::Absent
        ));
        assert!(!fresh_dir.join("plan.json").exists());
    }

    /// v2 registers goal and cron `undoable: false`: undoing a turn must not
    /// rewind goal progress or cron schedules, while todo/task/turn/plan do
    /// rewind. The checkpoint stack is the carrier of that distinction here.
    #[test]
    fn test_rollback_preserves_non_undoable_domains() {
        let (_tmp, store) = store();

        let todo_before = json!([{ "id": "T1", "title": "Read" }]);
        let goal_before = json!({ "goal": { "goalId": "g1", "turnsUsed": 1 } });
        let cron_before = json!([{ "id": "c1", "schedule": "@daily" }]);
        store.write_domain("todo", &todo_before).unwrap();
        store.write_domain("goal", &goal_before).unwrap();
        store.write_domain("cron", &cron_before).unwrap();
        store.checkpoint().unwrap();

        let goal_after = json!({ "goal": { "goalId": "g1", "turnsUsed": 2 } });
        let cron_after = json!([{ "id": "c2", "schedule": "@hourly" }]);
        store.write_domain("todo", &json!([])).unwrap();
        store.write_domain("goal", &goal_after).unwrap();
        store.write_domain("cron", &cron_after).unwrap();

        assert!(store.rollback().unwrap());

        assert_eq!(
            store.read_domain_state("todo").into_result().unwrap(),
            Some(todo_before),
            "undoable domains are restored to the checkpoint"
        );
        assert_eq!(
            store.read_domain_state("goal").into_result().unwrap(),
            Some(goal_after),
            "goal progress is undoable:false in v2 and must survive undo"
        );
        assert_eq!(
            store.read_domain_state("cron").into_result().unwrap(),
            Some(cron_after),
            "cron schedules are undoable:false in v2 and must survive undo"
        );
    }

    /// Two in-process writers of one domain must not share a staging file:
    /// the per-turn pipeline and the task runner both write here.
    #[test]
    fn test_concurrent_writers_of_one_domain_do_not_share_a_tmp_file() {
        let (_tmp, store) = store();

        // The staging name is unique per call, not a shared `<domain>.json.tmp`,
        // and it stays in the target's directory so the rename is atomic.
        let todo = store.state_dir.join("todo.json");
        let first = unique_tmp_path(&todo);
        let second = unique_tmp_path(&todo);
        assert_ne!(first, second, "two writers must never stage in one file");
        assert_eq!(
            first.parent(),
            todo.parent(),
            "the staging file must sit next to its target"
        );
        assert!(
            !first.starts_with("todo.json.tmp"),
            "the old fixed temp name is exactly the collision: {}",
            first.display()
        );

        let big_a = json!([{ "id": "T1", "title": "a".repeat(200_000) }]);
        let big_b = json!([{ "id": "T2", "title": "b".repeat(200_000) }]);
        std::thread::scope(|scope| {
            for value in [&big_a, &big_b, &big_a, &big_b] {
                let store = &store;
                scope.spawn(move || store.write_domain("todo", value).unwrap());
            }
        });
        // Whatever won the last rename, the file is one complete value.
        let read = store.read_domain_state("todo");
        let value = read
            .into_result()
            .expect("the file parses after concurrent writes")
            .expect("the file is present after concurrent writes");
        let id = value[0]["id"].as_str().expect("a complete todo entry");
        assert!(id == "T1" || id == "T2", "torn write: {value}");
        let leftovers: Vec<_> = fs::read_dir(store.state_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp files left behind: {leftovers:?}");
    }

    #[test]
    fn test_undo_checkpoint_survives_restart() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let todo = json!([{ "id": "T1", "status": "in_progress" }]);
        // Session 1: write a todo, then checkpoint.
        {
            let store = StateStore::for_dir(state_dir.clone()).unwrap();
            store.write_domain("todo", &todo).unwrap();
            store.checkpoint().unwrap();
            assert_eq!(store.checkpoint_depth(), 1);
        }
        // Session 2 (restart): re-open the same dir. Checkpoint file
        // survives, depth reflects it. Then write a different todo and
        // roll back — the pre-checkpoint value must come back.
        {
            let store = StateStore::for_dir(state_dir.clone()).unwrap();
            assert_eq!(
                store.checkpoint_depth(),
                1,
                "checkpoint stack must persist across restart"
            );
            let new = json!([{ "id": "T2", "status": "completed" }]);
            store.write_domain("todo", &new).unwrap();
            assert_eq!(
                store.read_domain_state("todo").into_result().unwrap(),
                Some(new.clone())
            );
            assert!(store.rollback().unwrap());
            assert_eq!(
                store.read_domain_state("todo").into_result().unwrap(),
                Some(todo.clone()),
                "rollback restores the pre-checkpoint value"
            );
            assert_eq!(store.checkpoint_depth(), 0);
        }
        // Session 3 (another restart): the rollback consumed the file,
        // so a fresh open sees depth 0.
        {
            let store = StateStore::for_dir(state_dir).unwrap();
            assert_eq!(store.checkpoint_depth(), 0);
        }
    }

    #[test]
    fn test_undo_checkpoint_stack_ordering_across_restart() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        // Session 1: two checkpoints, no rollbacks.
        {
            let store = StateStore::for_dir(state_dir.clone()).unwrap();
            store.checkpoint().unwrap();
            store.checkpoint().unwrap();
            assert_eq!(store.checkpoint_depth(), 2);
        }
        // Session 2: open, roll back once — depth 1.
        {
            let store = StateStore::for_dir(state_dir.clone()).unwrap();
            assert_eq!(store.checkpoint_depth(), 2);
            assert!(store.rollback().unwrap());
            assert_eq!(store.checkpoint_depth(), 1);
        }
        // Session 3: open, roll back again — depth 0.
        {
            let store = StateStore::for_dir(state_dir).unwrap();
            assert_eq!(store.checkpoint_depth(), 1);
            assert!(store.rollback().unwrap());
            assert_eq!(store.checkpoint_depth(), 0);
        }
    }

    #[test]
    fn test_checkpoint_stack_is_capped_at_the_v2_window() {
        let tmp = TempDir::new().unwrap();
        let state_dir = tmp.path().join("state");
        let store = StateStore::for_dir(state_dir.clone()).unwrap();
        for _ in 0..(STATE_CHECKPOINT_MAX_ENTRIES + 5) {
            store.checkpoint().unwrap();
        }
        assert_eq!(
            store.checkpoint_depth(),
            STATE_CHECKPOINT_MAX_ENTRIES,
            "the stack must not grow past the v2 window"
        );
        // The newest snapshots are the ones kept: the oldest five are gone.
        let dir = state_dir.join("checkpoints");
        assert!(!checkpoint_path(&dir, 1).exists());
        assert!(!checkpoint_path(&dir, 5).exists());
        assert!(checkpoint_path(&dir, 6).exists());
        assert!(checkpoint_path(&dir, 25).exists());
        // Undo still works against the retained window.
        assert!(store.rollback().unwrap());
        assert_eq!(store.checkpoint_depth(), STATE_CHECKPOINT_MAX_ENTRIES - 1);
    }

    #[test]
    fn test_clear_domain_restores_default() {
        let (_tmp, store) = store();
        store
            .write_domain(
                "todo",
                &json!([{ "id": "T1", "title": "A", "status": "pending" }]),
            )
            .unwrap();
        store.clear_domain("todo").unwrap();
        assert_eq!(store.read_domain_state("todo").into_result().unwrap(), None);
        assert_eq!(store.read_state("todo", "todo").unwrap(), json!([]));
        // Clearing an already-empty domain is a no-op, not an error.
        store.clear_domain("cron").unwrap();
    }

    #[test]
    fn test_unknown_domain_errors() {
        let (_tmp, store) = store();
        let read = store.read_state("skill", "commit").unwrap_err();
        assert!(read.contains("-32001"));
        assert!(read.contains("unknown state domain: skill"));
        let write = store.write_domain("skill", &json!({})).unwrap_err();
        assert!(write.contains("unknown state domain: skill"));
        let apply = store.apply_write("skill", &json!({})).unwrap_err();
        assert!(apply.contains("-32001"));
    }

    #[test]
    fn test_plan_enter_exit() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write("plan", &json!({ "active": true }))
            .unwrap();
        assert_eq!(outcome.stored["active"], true);
        assert!(outcome.stored["id"].is_string());
        assert!(outcome.stored["path"].is_string());
        assert!(outcome.stored["path"].as_str().unwrap().ends_with(".md"));
        store.write_domain("plan", &outcome.stored).unwrap();
        // A second enter while active is rejected (-32004).
        let err = store
            .apply_write("plan", &json!({ "active": true }))
            .unwrap_err();
        assert!(err.contains("-32004"));
        // Exit deactivates and clears the id/path.
        let outcome = store
            .apply_write("plan", &json!({ "active": false }))
            .unwrap();
        assert_eq!(outcome.stored, json!({ "active": false }));
        store.write_domain("plan", &outcome.stored).unwrap();
        assert_eq!(
            store.read_state("plan", "plan").unwrap(),
            json!({ "active": false })
        );
        // Invalid values are rejected (-32003).
        let err = store
            .apply_write("plan", &json!({ "active": "yes" }))
            .unwrap_err();
        assert!(err.contains("-32003"));
    }

    #[test]
    fn test_goal_create_update_set_budget() {
        let (_tmp, store) = store();
        // Create.
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature", "completion_criterion": "Tests pass" }),
            )
            .unwrap();
        let goal = &outcome.stored["goal"];
        assert_eq!(goal["objective"], "Ship the feature");
        assert_eq!(goal["completionCriterion"], "Tests pass");
        assert_eq!(goal["status"], "active");
        assert!(goal["goalId"].is_string());
        store.write_domain("goal", &outcome.stored).unwrap();
        // A second create is rejected (-32004).
        let err = store
            .apply_write("goal", &json!({ "action": "create", "objective": "Again" }))
            .unwrap_err();
        assert!(err.contains("-32004"));
        // Update to complete.
        let outcome = store
            .apply_write("goal", &json!({ "action": "update", "status": "complete" }))
            .unwrap();
        assert_eq!(outcome.stored["goal"]["status"], "complete");
        store.write_domain("goal", &outcome.stored).unwrap();
        // Resuming a completed goal is rejected (-32004).
        let err = store
            .apply_write("goal", &json!({ "action": "update", "status": "active" }))
            .unwrap_err();
        assert!(err.contains("-32004"));
        // Set a budget on the completed goal.
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "set_budget", "value": 20, "unit": "turns" }),
            )
            .unwrap();
        assert_eq!(outcome.response["goal"]["budget"]["turnBudget"], 20);
        store.write_domain("goal", &outcome.stored).unwrap();
        // Update with no goal at all.
        store.clear_domain("goal").unwrap();
        let err = store
            .apply_write("goal", &json!({ "action": "update", "status": "complete" }))
            .unwrap_err();
        assert!(err.contains("-32004"));
        assert!(err.contains("no active goal"));
        let err = store
            .apply_write(
                "goal",
                &json!({ "action": "set_budget", "value": 5, "unit": "turns" }),
            )
            .unwrap_err();
        assert!(err.contains("-32004"));
        // Unreasonable time budgets are rejected (-32003). Only the 1s floor
        // applies: the former 24h ceiling is gone, so long durations pass.
        let err = store
            .apply_write(
                "goal",
                &json!({ "action": "set_budget", "value": 0.5, "unit": "seconds" }),
            )
            .unwrap_err();
        assert!(err.contains("-32003"));
    }

    #[test]
    fn test_cron_create_delete() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "cron",
                &json!({ "action": "create", "cron": "0 9 * * *", "prompt": "daily check", "recurring": true }),
            )
            .unwrap();
        // The response is the created entry; the stored value is the list.
        assert_eq!(outcome.response["cron"], "0 9 * * *");
        assert_eq!(outcome.response["prompt"], "daily check");
        assert_eq!(outcome.response["recurring"], true);
        let id = outcome.response["id"].as_str().unwrap().to_string();
        assert_eq!(id.len(), 26);
        assert_eq!(outcome.stored.as_array().unwrap().len(), 1);
        store.write_domain("cron", &outcome.stored).unwrap();
        // Delete the created job.
        let outcome = store
            .apply_write("cron", &json!({ "action": "delete", "id": id }))
            .unwrap();
        assert_eq!(outcome.stored, json!([]));
        store.write_domain("cron", &outcome.stored).unwrap();
        // Deleting an unknown id is rejected (-32004).
        let err = store
            .apply_write(
                "cron",
                &json!({ "action": "delete", "id": "01ABCDEFGHJKMNPQRSTVWXYZ" }),
            )
            .unwrap_err();
        assert!(err.contains("-32004"));
        // Invalid actions are rejected (-32003).
        let err = store
            .apply_write("cron", &json!({ "action": "pause" }))
            .unwrap_err();
        assert!(err.contains("-32003"));
    }

    #[test]
    fn test_task_stop_wait_output() {
        let (_tmp, store) = store();
        let task = json!({
            "taskId": "task-1",
            "description": "run tests",
            "status": "running",
            "startedAt": 1,
        });
        store.write_domain("task", &json!([task])).unwrap();
        // TaskOutput-style read by task id.
        let value = store.read_state("task", "task-1").unwrap();
        assert_eq!(value["taskId"], "task-1");
        assert_eq!(value["status"], "running");
        // Unknown task id is a -32002 error.
        let err = store.read_state("task", "nope").unwrap_err();
        assert!(err.contains("-32002"));
        // Stop marks the task killed.
        let outcome = store
            .apply_write("task", &json!({ "action": "stop", "id": "task-1" }))
            .unwrap();
        assert_eq!(outcome.response["status"], "killed");
        assert_eq!(outcome.response["stopReason"], "Stopped by TaskStop");
        store.write_domain("task", &outcome.stored).unwrap();
        // Wait on the terminal task returns its entry.
        let outcome = store
            .apply_write(
                "task",
                &json!({ "action": "wait", "id": "task-1", "timeout_ms": 5000 }),
            )
            .unwrap();
        assert_eq!(outcome.response["status"], "killed");
        // Stopping an unknown task is a -32002 error.
        let err = store
            .apply_write("task", &json!({ "action": "stop", "id": "nope" }))
            .unwrap_err();
        assert!(err.contains("-32002"));
    }

    #[test]
    fn test_ulid_shape() {
        let id = ulid();
        assert_eq!(id.len(), 26);
        assert!(id.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert!(!id.contains('I') && !id.contains('L') && !id.contains('O') && !id.contains('U'));
        assert_ne!(ulid(), id);
    }

    #[test]
    fn test_goal_context_none_without_goal() {
        let (_tmp, store) = store();
        assert!(store.goal_context().is_none());
    }

    #[test]
    fn test_goal_context_projects_active_goal_with_live_wall_clock() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let ctx = store.goal_context().unwrap();
        assert!(ctx.goal_id.starts_with("goal-"));
        assert_eq!(ctx.objective, "Ship the feature");
        assert_eq!(ctx.status, crate::turn_loop::types::GoalStatus::Active);
        assert_eq!(ctx.tokens_used, 0);
        assert_eq!(ctx.turns_used, 0);
        assert!(ctx.wall_clock_ms >= 0);
        assert!(ctx.token_budget.is_none());
        assert!(ctx.turn_budget.is_none());
    }

    #[test]
    fn test_goal_context_maps_budgets_and_terminal_status() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "set_budget", "value": 20, "unit": "turns" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write("goal", &json!({ "action": "update", "status": "complete" }))
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let ctx = store.goal_context().unwrap();
        assert_eq!(ctx.status, crate::turn_loop::types::GoalStatus::Complete);
        assert_eq!(ctx.turn_budget, Some(20));
        assert!(ctx.token_budget.is_none());
    }

    #[test]
    fn test_goal_record_usage_accumulates_across_turns() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        for tokens in [10, 20] {
            let outcome = store
                .apply_write(
                    "goal",
                    &json!({ "action": "record_usage", "turns": 1, "output_tokens": tokens }),
                )
                .unwrap();
            store.write_domain("goal", &outcome.stored).unwrap();
        }
        let ctx = store.goal_context().unwrap();
        assert_eq!(ctx.turns_used, 2);
        assert_eq!(ctx.tokens_used, 30);
    }

    #[test]
    fn test_goal_record_usage_skips_inactive_goal() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write("goal", &json!({ "action": "update", "status": "complete" }))
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 10 }),
            )
            .unwrap();
        assert_eq!(outcome.stored["goal"]["status"], "complete");
        let ctx = store.goal_context().unwrap();
        assert_eq!(ctx.turns_used, 0);
        assert_eq!(ctx.tokens_used, 0);
    }

    /// v2 `blockIfBudgetReached`: the fold that reaches a limit blocks the
    /// goal in the same write, with the v2 terminal reason.
    #[test]
    fn test_goal_record_usage_blocks_when_the_budget_is_spent() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "set_budget", "value": 2, "unit": "turns" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();

        let first = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 5 }),
            )
            .unwrap();
        store.write_domain("goal", &first.stored).unwrap();
        assert_eq!(first.stored["goal"]["status"], "active");

        let second = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 5 }),
            )
            .unwrap();
        store.write_domain("goal", &second.stored).unwrap();
        assert_eq!(second.stored["goal"]["status"], "blocked");
        assert_eq!(
            second.stored["goal"]["terminalReason"],
            "Blocked after goal budget reached: turn budget 2"
        );
        let ctx = store.goal_context().unwrap();
        assert_eq!(ctx.status, crate::turn_loop::types::GoalStatus::Blocked);
    }

    /// v2 `setBudgetLimits`: setting a limit that is already spent blocks the
    /// goal immediately.
    #[test]
    fn test_goal_set_budget_blocks_an_already_spent_budget() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 50 }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "set_budget", "value": 25, "unit": "tokens" }),
            )
            .unwrap();
        assert_eq!(outcome.stored["goal"]["status"], "blocked");
        assert_eq!(
            outcome.stored["goal"]["terminalReason"],
            "Blocked after goal budget reached: token budget 25"
        );
    }

    /// v2 lifecycle transitions: the runtime `lifecycle` action settles the
    /// wall clock and records the reason, while the user-facing `update`
    /// action keeps `pauseGoal` / `resumeGoal` validation.
    #[test]
    fn test_goal_lifecycle_pauses_blocks_and_resumes() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Ship the feature" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();

        let paused = store
            .apply_write(
                "goal",
                &json!({ "action": "lifecycle", "status": "paused", "reason": "Paused after interruption" }),
            )
            .unwrap();
        store.write_domain("goal", &paused.stored).unwrap();
        assert_eq!(paused.stored["goal"]["status"], "paused");
        assert_eq!(
            paused.stored["goal"]["terminalReason"],
            "Paused after interruption"
        );

        // A second pause is a no-op that keeps the recorded reason.
        let again = store
            .apply_write(
                "goal",
                &json!({ "action": "lifecycle", "status": "paused", "reason": "other" }),
            )
            .unwrap();
        assert_eq!(
            again.stored["goal"]["terminalReason"],
            "Paused after interruption"
        );

        // Resume, then block through the runtime action: lifecycle transitions
        // only apply while the goal is active.
        let resumed = store
            .apply_write("goal", &json!({ "action": "update", "status": "active" }))
            .unwrap();
        store.write_domain("goal", &resumed.stored).unwrap();
        let blocked = store
            .apply_write(
                "goal",
                &json!({ "action": "lifecycle", "status": "blocked", "reason": "Blocked by UserPromptSubmit hook" }),
            )
            .unwrap();
        store.write_domain("goal", &blocked.stored).unwrap();
        assert_eq!(blocked.stored["goal"]["status"], "blocked");
        assert_eq!(
            blocked.stored["goal"]["terminalReason"],
            "Blocked by UserPromptSubmit hook"
        );

        // The user cannot pause a blocked goal, but can resume it.
        let err = store
            .apply_write("goal", &json!({ "action": "update", "status": "paused" }))
            .unwrap_err();
        assert!(
            err.contains("Cannot pause a goal in status \"blocked\""),
            "{err}"
        );

        let resumed = store
            .apply_write("goal", &json!({ "action": "update", "status": "active" }))
            .unwrap();
        store.write_domain("goal", &resumed.stored).unwrap();
        assert_eq!(resumed.stored["goal"]["status"], "active");
        assert!(
            resumed.response["goal"]
                .get("terminalReason")
                .is_none_or(serde_json::Value::is_null)
        );
    }

    /// The persisted goal record must keep `wallClockResumedAt`; storing the
    /// read-projection instead froze the live wall clock at the last write,
    /// so a wall-clock budget could never be reached by elapsed time.
    #[test]
    fn test_goal_wall_clock_accumulates_from_the_persisted_anchor() {
        let (_tmp, store) = store();
        let now = now_ms();
        store
            .write_domain(
                "goal",
                &json!({ "goal": {
                    "goalId": "goal-clock",
                    "objective": "o",
                    "status": "active",
                    "turnsUsed": 0,
                    "tokensUsed": 0,
                    "inputTokensUsed": 0,
                    "outputTokensUsed": 0,
                    "wallClockMs": 1000,
                    "wallClockResumedAt": now.saturating_sub(2500),
                    "createdAt": now.saturating_sub(5000),
                    "updatedAt": now.saturating_sub(500)
                }}),
            )
            .unwrap();
        let ctx = store.goal_context().unwrap();
        assert!(
            ctx.wall_clock_ms >= 3500,
            "live wall clock: {}",
            ctx.wall_clock_ms
        );

        // A write round-trips the anchor instead of settling it.
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 0 }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        let after = store
            .read_goal_state()
            .expect("the goal domain must be readable")
            .expect("still stored");
        assert!(after.wall_clock_resumed_at.is_some());
    }

    /// v2 `resumePersisted` (`test/wire/resume.test.ts:850-911`): a goal that
    /// was active when the engine stopped is paused on resume, and the gap the
    /// engine was not running is not charged to its wall clock — otherwise a
    /// wall-clock budget could come back already spent after an overnight
    /// shutdown.
    #[test]
    fn test_goal_paused_on_resume_does_not_charge_offline_time() {
        let (_tmp, store) = store();
        // Active for 2 500 ms of *online* time, but the anchor is 60 s old:
        // reading it would charge 60 s, of which 57.5 s are offline.
        let now = now_ms();
        store
            .write_domain(
                "goal",
                &json!({ "goal": {
                    "goalId": "goal-resume",
                    "objective": "ship it",
                    "status": "active",
                    "turnsUsed": 0,
                    "tokensUsed": 0,
                    "inputTokensUsed": 0,
                    "outputTokensUsed": 0,
                    "wallClockMs": 2500,
                    "wallClockResumedAt": now.saturating_sub(60_000),
                    "budget": { "wallClockBudgetMs": 10_000 },
                    "createdAt": now.saturating_sub(120_000),
                    "updatedAt": now.saturating_sub(60_000)
                }}),
            )
            .unwrap();

        let paused = store
            .pause_goal_on_resume()
            .expect("an active goal is paused on resume");
        assert_eq!(paused.wall_clock_ms, 2500, "offline time is not charged");
        assert!(!paused.budget.wall_clock_budget_reached);

        let state = store
            .read_goal_state()
            .expect("the goal domain must be readable")
            .expect("still stored");
        assert_eq!(state.status, GoalStatus::Paused);
        assert_eq!(state.wall_clock_ms, 2500);
        assert!(
            state.wall_clock_resumed_at.is_none(),
            "the stale anchor is cleared so a later resume starts fresh"
        );

        // Nothing to do on a second resume, and a paused goal is left alone.
        assert!(store.pause_goal_on_resume().is_none());
    }

    /// Only a goal the host has reported back can be cleared by a host that
    /// now reports none; an engine-created goal the host has not adopted yet
    /// must survive.
    #[test]
    fn test_clear_host_goal_only_clears_an_adopted_goal() {
        let (_tmp, store) = store();
        let outcome = store
            .apply_write(
                "goal",
                &json!({ "action": "create", "objective": "Engine goal" }),
            )
            .unwrap();
        store.write_domain("goal", &outcome.stored).unwrap();
        assert!(!store.clear_host_goal().unwrap());
        assert!(store.goal_context().is_some());

        let context = crate::rpc::types::GoalContext {
            goal_id: "goal-host".into(),
            objective: "Host goal".into(),
            status: crate::rpc::types::GoalStatus::Active,
            token_budget: None,
            turn_budget: None,
            wall_clock_budget_ms: None,
            wall_clock_ms: 0,
            tokens_used: 0,
            turns_used: 0,
            updated_at: 0,
            terminal_reason: None,
        };
        assert!(store.sync_goal_from_context(&context).unwrap());
        assert!(store.clear_host_goal().unwrap());
        assert!(store.goal_context().is_none());
    }

    #[test]
    fn test_task_output_round_trip() {
        let (_tmp, store) = store();
        assert!(store.read_task_output("task-1", 1024).is_none());
        store.write_task_output("task-1", "line one\nline two\n");
        let snapshot = store.read_task_output("task-1", 1024).unwrap();
        assert_eq!(snapshot["outputSizeBytes"], 18);
        assert_eq!(snapshot["previewBytes"], 18);
        assert_eq!(snapshot["truncated"], false);
        assert_eq!(snapshot["preview"], "line one\nline two\n");
        assert!(
            snapshot["outputPath"]
                .as_str()
                .unwrap()
                .replace('\\', "/")
                .ends_with("tasks/task-1/output.log")
        );
    }

    #[test]
    fn test_task_output_truncated_preview_takes_tail() {
        let (_tmp, store) = store();
        let output = "x".repeat(40 * 1024);
        store.write_task_output("task-1", &output);
        let snapshot = store.read_task_output("task-1", 1024).unwrap();
        assert_eq!(snapshot["outputSizeBytes"], 40 * 1024);
        assert_eq!(snapshot["previewBytes"], 1024);
        assert_eq!(snapshot["truncated"], true);
        assert_eq!(snapshot["preview"], "x".repeat(1024));
    }

    #[test]
    fn test_read_state_task_attaches_output_snapshot() {
        let (_tmp, store) = store();
        store
            .write_domain(
                "task",
                &json!([{ "taskId": "task-1", "description": "build", "status": "completed", "startedAt": 1, "endedAt": 2 }]),
            )
            .unwrap();
        store.write_task_output("task-1", "build output");
        let entry = store.read_state("task", "task-1").unwrap();
        assert_eq!(entry["taskId"], "task-1");
        assert_eq!(entry["status"], "completed");
        assert_eq!(entry["preview"], "build output");
        assert_eq!(entry["truncated"], false);
        assert_eq!(entry["fullOutputAvailable"], true);
        let list = store.read_state("task", "task").unwrap();
        assert!(list.as_array().unwrap()[0].get("preview").is_none());
    }

    // ── persisted task output ceiling (v2 MAX_TASK_OUTPUT_BYTES) ────────

    /// The cap exists because this store is handed one finished `String`: a
    /// task that produced far too much output used to write all of it.
    #[test]
    fn write_task_output_caps_at_the_persisted_ceiling() {
        let (_tmp, store) = store();
        let payload = "z".repeat(MAX_PERSISTED_TASK_OUTPUT_BYTES + 8192);
        store.write_task_output("task-big", &payload);

        let written = std::fs::read_to_string(store.task_output_path("task-big")).unwrap();
        assert!(
            written.len() <= MAX_PERSISTED_TASK_OUTPUT_BYTES + 4,
            "the log must be capped, not merely annotated: {} bytes",
            written.len()
        );
        assert!(
            written.chars().all(|c| c == 'z'),
            "a capped log is still valid UTF-8 — the cut is on a char boundary"
        );
    }

    /// v2 keeps the tail (`appendRetainedOutput`,
    /// `agent/task/taskService.ts:1001-1019`), because the end of a command's
    /// output is what answers what happened.
    #[test]
    fn the_persisted_cap_keeps_the_tail() {
        let mut payload = "a".repeat(MAX_PERSISTED_TASK_OUTPUT_BYTES + 4096);
        payload.push_str("THE-END");
        let capped = cap_output_tail(&payload);
        assert!(
            capped.ends_with("THE-END"),
            "the marker must survive: {:?}",
            &capped[capped.len().saturating_sub(16)..]
        );
        assert!(capped.len() <= MAX_PERSISTED_TASK_OUTPUT_BYTES + 4);
    }

    /// Output under the cap is written whole — the cap must not truncate normal
    /// work, or every long build would lose its ending.
    #[test]
    fn output_under_the_cap_is_written_whole() {
        let short = "a perfectly ordinary build log".repeat(1000);
        assert_eq!(cap_output_tail(&short), short.as_str());
        assert_eq!(cap_output_tail(""), "");
    }

    /// A multi-byte character straddling the cut must not be sliced in half —
    /// that would put invalid UTF-8 in a file the reader parses.
    #[test]
    fn the_cap_cuts_on_a_character_boundary() {
        // 'é' is two bytes; land the cut in the middle of one.
        let payload: String = "é".repeat(MAX_PERSISTED_TASK_OUTPUT_BYTES / 2 + 64);
        let capped = cap_output_tail(&payload);
        assert!(
            std::str::from_utf8(capped.as_bytes()).is_ok(),
            "the capped slice must stay valid UTF-8"
        );
        assert!(capped.len() <= MAX_PERSISTED_TASK_OUTPUT_BYTES);
    }

    #[test]
    fn test_fold_turn_event_advances_clock_once_per_prompt() {
        use crate::turn_events::TurnEndReason;
        let (_tmp, store) = store();
        assert_eq!(store.read_state("turn", "").unwrap()["nextTurnId"], 0);
        for turn_id in 0..3u64 {
            store.fold_turn_event(&TurnEvent::Prompt {
                turn_id,
                prompt_id: None,
                input: json!([]),
                origin: json!({ "kind": "user" }),
            });
            store.fold_turn_event(&TurnEvent::Started {
                turn_id,
                prompt_id: None,
                origin: json!({ "kind": "user" }),
                prompt: None,
                prompt_attachments: None,
            });
            store.fold_turn_event(&TurnEvent::Ended {
                turn_id,
                reason: TurnEndReason::Completed,
                error: None,
                duration_ms: Some(5),
                interrupt_reason: None,
                trace_id: None,
                stop_reason: None,
            });
        }
        let state = store.read_state("turn", "").unwrap();
        assert_eq!(state["nextTurnId"], 3, "one clock step per prompt");
        assert_eq!(state["lastEnded"]["turnId"], 2);
        assert_eq!(state["lastEnded"]["reason"], "completed");
    }

    #[test]
    fn test_fold_turn_event_reserves_only_unconsumed_queued_ids() {
        let (_tmp, store) = store();
        store
            .write_domain("turn", &json!({ "nextTurnId": 7, "cancelledTurnIds": [] }))
            .unwrap();
        store.fold_turn_event(&TurnEvent::Cancel {
            turn_id: Some(9),
            target: Some(TurnCancelTarget::Queued),
            reason: None,
        });
        // Already ran (id below the clock) — the clock must not grow backwards.
        store.fold_turn_event(&TurnEvent::Cancel {
            turn_id: Some(3),
            target: Some(TurnCancelTarget::Queued),
            reason: None,
        });
        // An active turn is accounted for by turn.ended, not by the clock.
        store.fold_turn_event(&TurnEvent::Cancel {
            turn_id: Some(9),
            target: Some(TurnCancelTarget::Active),
            reason: None,
        });
        let state = store.read_state("turn", "").unwrap();
        assert_eq!(state["cancelledTurnIds"], json!([9]));
        assert_eq!(state["nextTurnId"], 7);
    }

    #[test]
    fn test_turn_write_never_rewinds_clock() {
        let (_tmp, store) = store();
        store
            .write_domain(
                "turn",
                &json!({ "nextTurnId": 12, "cancelledTurnIds": [4] }),
            )
            .unwrap();
        let outcome = store
            .apply_write("turn", &json!({ "nextTurnId": 3 }))
            .expect("a lower clock is accepted, clamped");
        assert_eq!(
            outcome.stored["nextTurnId"], 12,
            "a stale write must not hand out an id that was already used"
        );
        assert!(
            outcome.stored.get("cancelledTurnIds").is_none(),
            "fields the write omits are cleared"
        );
        let err = store.apply_write("turn", &json!({})).unwrap_err();
        assert!(err.contains("-32003"), "{err}");
    }

    #[test]
    fn test_state_store_patch_and_diff_domain() {
        let (_tmp, store) = store();
        store
            .write_domain(
                "plan",
                &json!({ "active": true, "id": "p-1", "path": "/path/p-1.md" }),
            )
            .unwrap();

        let diff = store
            .diff_domain(
                "plan",
                &json!({ "active": false, "id": "p-1", "path": "/path/p-1.md" }),
            )
            .unwrap();
        assert!(!diff.is_empty());

        let (patched, inverse) = store.patch_domain("plan", &diff).unwrap();
        assert_eq!(patched["active"], false);

        // Undo using inverse patch
        let (reverted, _) = store.patch_domain("plan", &inverse).unwrap();
        assert_eq!(reverted["active"], true);
    }

    /// Native undo unwinds one snapshot per removed turn and never touches
    /// another session's stack.
    #[test]
    fn rollback_scoped_state_unwinds_only_the_given_sessions_stack() {
        let workspace = TempDir::new().unwrap();
        let store = StateStore::for_session(workspace.path(), "session-a").unwrap();
        let first = json!([{ "id": "T1", "title": "first" }]);
        store.write_domain("todo", &first).unwrap();
        store.checkpoint().unwrap();
        let second = json!([{ "id": "T2", "title": "second" }]);
        store.write_domain("todo", &second).unwrap();
        store.checkpoint().unwrap();
        store
            .write_domain("todo", &json!([{ "id": "T3", "title": "third" }]))
            .unwrap();

        // One undone turn restores the second snapshot, not the first.
        assert_eq!(rollback_scoped_state(workspace.path(), "session-a", 1), 1);
        assert_eq!(
            store.read_domain_state("todo").into_result().unwrap(),
            Some(second.clone())
        );
        assert_eq!(rollback_scoped_state(workspace.path(), "session-a", 1), 1);
        assert_eq!(
            store.read_domain_state("todo").into_result().unwrap(),
            Some(first)
        );
        // The stack is exhausted now: an over-deep undo stops quietly.
        assert_eq!(rollback_scoped_state(workspace.path(), "session-a", 5), 0);
        // Another session of the same workspace has its own stack.
        assert_eq!(rollback_scoped_state(workspace.path(), "session-b", 1), 0);
    }

    /// A `task.json` that parses but is not a list is malformed, not empty.
    /// Reading it as "no tasks" made every task look absent and let the next
    /// write replace the file, so both the read and the write must fail and
    /// leave the bytes alone.
    #[test]
    fn a_malformed_task_list_is_refused_instead_of_read_as_empty() {
        let (_tmp, store) = store();
        let path = store.domain_file("task").unwrap();
        let original = json!({ "taskId": "task-1", "status": "running" });
        fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        let read = store.read_state("task", "task-1").unwrap_err();
        assert!(read.contains("malformed"), "{read}");

        let write = store
            .apply_write_and_persist("task", &json!({ "action": "stop", "id": "task-1" }))
            .unwrap_err();
        assert!(write.contains("malformed"), "{write}");

        // The refused write must not have replaced the only copy.
        let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after, original);
    }

    /// Same rule for `cron.json`: a wrong-shaped file is an error, not an
    /// empty schedule that the next create/delete overwrites.
    #[test]
    fn a_malformed_cron_list_is_refused_instead_of_replaced() {
        let (_tmp, store) = store();
        let path = store.domain_file("cron").unwrap();
        let original = json!({ "entries": [] });
        fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        let create = store
            .apply_write_and_persist(
                "cron",
                &json!({ "action": "create", "cron": "*/5 * * * *", "prompt": "tick" }),
            )
            .unwrap_err();
        assert!(create.contains("malformed"), "{create}");
        let delete = store
            .apply_write_and_persist("cron", &json!({ "action": "delete", "id": "cron-1" }))
            .unwrap_err();
        assert!(delete.contains("malformed"), "{delete}");

        let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after, original);
    }

    /// An unlistable checkpoint stack reports zero (the host has no error
    /// channel) but must not take the process down.
    #[test]
    fn checkpoint_depth_reports_zero_when_the_stack_cannot_be_listed() {
        let (_tmp, store) = store();
        store.checkpoint().unwrap();
        assert_eq!(store.checkpoint_depth(), 1);
        fs::remove_dir_all(&store.checkpoints_dir).unwrap();
        assert_eq!(store.checkpoint_depth(), 0);
    }
}

#[cfg(test)]
mod goal_usage_tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, StateStore) {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
        (tmp, store)
    }

    /// The per-turn fold the turn driver submits: +1 turn and the output tokens
    /// of the finished turn, into an active goal only.
    #[test]
    fn record_usage_folds_turns_and_output_tokens_into_an_active_goal() {
        let (_tmp, store) = store();
        // A session without a goal is the common case: a no-op, not an error.
        let none = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 10 }),
            )
            .unwrap();
        assert!(none.stored["goal"].is_null());

        let created = store
            .apply_write("goal", &json!({ "action": "create", "objective": "Ship" }))
            .unwrap();
        store.write_domain("goal", &created.stored).unwrap();

        let folded = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 7 }),
            )
            .unwrap();
        store.write_domain("goal", &folded.stored).unwrap();
        assert_eq!(folded.stored["goal"]["turnsUsed"], 1);
        assert_eq!(folded.stored["goal"]["tokensUsed"], 7);
        assert_eq!(folded.stored["goal"]["outputTokensUsed"], 7);

        // A terminal goal's counters freeze: the fold still answers with the
        // snapshot, so the caller sees the goal is no longer active.
        let completed = store
            .apply_write("goal", &json!({ "action": "update", "status": "complete" }))
            .unwrap();
        store.write_domain("goal", &completed.stored).unwrap();
        let frozen = store
            .apply_write(
                "goal",
                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 5 }),
            )
            .unwrap();
        assert_eq!(frozen.stored["goal"]["turnsUsed"], 1);
        assert_eq!(frozen.stored["goal"]["status"], "complete");
    }

    /// The host creates goals; the engine adopts the snapshot the first time
    /// it sees it, then only applies newer host transitions — a host snapshot
    /// that lags the engine's last fold must not roll counters back, and a
    /// host pause / resume / cancel must land in the store before the next
    /// continuation decision reads it.
    #[test]
    fn sync_goal_from_context_adopts_and_reconciles_the_host_snapshot() {
        let (_tmp, store) = store();
        let now = now_ms();
        let mut context = crate::rpc::types::GoalContext {
            goal_id: "goal-host".into(),
            objective: "Host goal".into(),
            status: crate::rpc::types::GoalStatus::Active,
            token_budget: Some(5000),
            turn_budget: Some(3),
            wall_clock_budget_ms: None,
            wall_clock_ms: 1200,
            tokens_used: 42,
            turns_used: 2,
            updated_at: now as i64,
            terminal_reason: None,
        };
        assert!(store.sync_goal_from_context(&context).unwrap());
        let stored = store.goal_context().expect("seeded");
        assert_eq!(stored.goal_id, "goal-host");
        assert_eq!(stored.turns_used, 2);
        assert_eq!(stored.tokens_used, 42);
        assert_eq!(stored.turn_budget, Some(3));
        assert_eq!(stored.token_budget, Some(5000));

        // An older host snapshot (the host has not adopted the engine's last
        // fold yet) leaves the store untouched.
        context.status = crate::rpc::types::GoalStatus::Paused;
        context.tokens_used = 1;
        context.updated_at = now as i64 - 1_000;
        assert!(!store.sync_goal_from_context(&context).unwrap());
        let stored = store.goal_context().unwrap();
        assert_eq!(stored.status, crate::turn_loop::types::GoalStatus::Active);
        assert_eq!(stored.tokens_used, 42);

        // A newer host transition wins: pause, then resume. The reason the
        // host names rides the pause (v2 `GoalSnapshot.terminalReason`) — the
        // engine used to blank it, so a host-side pause reached the transcript
        // with no explanation.
        context.updated_at = now as i64 + 1_000;
        context.terminal_reason = Some("Paused by the user".into());
        assert!(store.sync_goal_from_context(&context).unwrap());
        let paused = store.goal_context().unwrap();
        assert_eq!(paused.status, crate::turn_loop::types::GoalStatus::Paused);
        assert_eq!(
            paused.terminal_reason.as_deref(),
            Some("Paused by the user")
        );

        context.status = crate::rpc::types::GoalStatus::Active;
        context.updated_at = now as i64 + 2_000;
        assert!(store.sync_goal_from_context(&context).unwrap());
        let resumed = store.goal_context().unwrap();
        assert_eq!(resumed.status, crate::turn_loop::types::GoalStatus::Active);
        assert_eq!(
            resumed.terminal_reason, None,
            "a goal that is running again has no terminal reason"
        );

        // A different goal id is a host replace: the store adopts it wholesale.
        context.goal_id = "goal-other".into();
        context.objective = "Other".into();
        context.updated_at = now as i64 - 5_000;
        assert!(store.sync_goal_from_context(&context).unwrap());
        assert_eq!(store.goal_context().unwrap().goal_id, "goal-other");
    }

    /// v2 #4568: the `objective` action edits in place. A no-op still returns
    /// a snapshot, a completed goal is terminal, and counters survive.
    #[test]
    fn edit_objective_action_changes_only_the_objective() {
        let (_tmp, store) = store();
        let created = store
            .apply_write("goal", &json!({ "action": "create", "objective": "old" }))
            .unwrap();
        store.write_domain("goal", &created.stored).unwrap();
        store
            .apply_write("goal", &json!({ "action": "record_usage", "turns": 2 }))
            .map(|outcome| store.write_domain("goal", &outcome.stored).unwrap())
            .unwrap();

        let edited = store
            .apply_write(
                "goal",
                &json!({ "action": "objective", "objective": "  new  " }),
            )
            .unwrap();
        store.write_domain("goal", &edited.stored).unwrap();
        assert_eq!(edited.response["goal"]["objective"], "new");
        assert_eq!(edited.response["goal"]["turnsUsed"], 2);
        let stored = store.goal_snapshot().unwrap();
        assert_eq!(stored["objective"], "new");
        assert_eq!(stored["turnsUsed"], 2);

        let unchanged = store
            .apply_write(
                "goal",
                &json!({ "action": "objective", "objective": "new" }),
            )
            .unwrap();
        assert_eq!(unchanged.response["goal"]["objective"], "new");
        assert!(
            store
                .apply_write("goal", &json!({ "action": "objective", "objective": " " }))
                .is_err(),
            "empty is refused"
        );
    }

    /// The turn pipeline and the task runner write the same goal domain from
    /// separate tasks. Two writers folding usage must not lose an update: the
    /// read-modify-write and the persist are one locked transaction via
    /// `apply_write_and_persist`, which is what the production callers use.
    #[test]
    fn concurrent_usage_folds_do_not_lose_a_write() {
        let (_tmp, store) = store();
        let store = std::sync::Arc::new(store);
        store
            .apply_write_and_persist("goal", &json!({ "action": "create", "objective": "Count" }))
            .unwrap();

        let workers: Vec<_> = (0..2)
            .map(|_| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        store
                            .apply_write_and_persist(
                                "goal",
                                &json!({ "action": "record_usage", "turns": 1, "output_tokens": 1 }),
                            )
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let stored = store
            .read_domain_state("goal")
            .into_result()
            .unwrap()
            .unwrap();
        assert_eq!(stored["goal"]["tokensUsed"], json!(100));
        assert_eq!(stored["goal"]["turnsUsed"], json!(100));
    }
}
