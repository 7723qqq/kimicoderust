
# v2 parity ledger

This is the live parity ledger. The historical `packages/kimi-agent/ROADMAP*.md`
records and the three root inventory reports were archived out of the tracked
tree on 2026-11-07; their still-live conclusions are carried here and in
`scripts/upstream-v2-delta-allowlist.json`. The legacy `check-roadmap-refs`
citation gate was retired with those files; citation integrity now rests on the
static gates in `bun run check:all` and this ledger's per-delta entries.

The upstream ratchet is triaged: `scripts/upstream-v2-delta-allowlist.json`
currently records 132 upstream deltas (77 `ported`, 55 `not-applicable`,
0 `tracked`, read on 2026-11-08). `check:upstream-v2-delta` exits 0. This is a
verdict against the user's fork at `D:\kimi\kimi-code`, not a byte-for-byte
claim about the Rust and TypeScript repositories: several newer upstream commits
are absent from that fork's checked-out behavior tree and are recorded as
`not-applicable` with source evidence.

The behavioral reference is now project-local: `MEMORY.md` records
`D:\kimi\kimi-code` (`github.com/7723qqq/kimi-code`) and the Bun 1.4.4 source
at `D:\kimi\bun`. `scripts/v2-parity-tool.ts` reads that fork's
`packages/agent-core-v2/src` by default; pass `--v2-root=<path>` to override it.
Its latest mapping scanned 1444 v2 files against 383 Rust files. A heuristic
mapping is not a behavior proof: the per-delta entries and tests below remain
the acceptance evidence.

## Upstream delta rulings (2026-11)

All five previously `tracked` deltas were resolved on 2026-11-08 against the
user's fork:

- `80193374eb` is `ported`: marketplace `sha256sum`, verified archive caching,
  digest mismatch refusal and the existing MCP OAuth cancel/invalidate paths are
  implemented and pinned by tests.
- `1dd5578649`, `2be062e8ea`, `aca85b92fc`, `ebc2a9f960` are `not-applicable`:
  the corresponding conversation-feed, remote-environment, requester-side
  cache-break and task-WS changes are not present in the checked-out reference
  fork's behavior tree, or depend on subsystems this single-process engine does
  not have. `2be062e8ea` still gains its local `environment-binding` debug route
  and a test, so the available subset is aligned.

### Overall Rust-port wave order (historical; re-derived 2026-11-07)

Engine-owned slices, in dependency order; each ends with a real-path test and a
verdict change from `tracked` to `ported` (or a recorded `not-applicable`):

1. **Tool schema reconciliation** — `df90d90ec0`, `bae32119ca`, `480019652d`: initial-snapshot reconciliation is landed (current schema for same-name tools, runtime-only MCP additions filtered when dynamic loading is off; `tools/toolset.rs`, `callbacks.rs`). **Closed 2026-11-08** — the four items this entry listed as open are implemented and pinned: pending same-name replacements are held in `tools/registry.rs` (`ToolRegistry::register` returns `RegistrationStatus::Pending`; `list` keeps serving the active definition while `list_live` reports the next session's view) and are reached from `mcp/manager.rs` (`has_pending_tool`, `list_live_tool_infos`); per-revision delta announcements are `tools/tool_select_state.rs::announcement_since`; the runtime-tool-change reminder and unsupported-dynamic-tool history shaping are `render_runtime_tool_change_reminder` / `shape_unsupported_dynamic_tool_history` in the same file; schema signatures are `registry.rs::{schema_signature, version_signature}`, consumed by `ToolSelectState::observe_live_updates`. Pinned by `a_changed_same_name_registration_stays_pending`, `schema_signature_ignores_object_key_order`, `version_signature_distinguishes_source_and_disclosure`, `announcement_walk_skips_changes_without_loadable_tools`, `runtime_reminder_uses_the_final_wording_and_keeps_both_blocks`, `unsupported_history_rewrites_a_prior_announcement`, `a_pending_update_queues_one_notice_per_signature`.
2. **Wire reader version** — `8fc5e22f96`, `f72dc7e1ca`: **ported**. `native/event_store/mod.rs` and `session/sqlite_store.rs` stamp `min_protocol_version` on every row and refuse readers below it with `WireReaderTooLow {required,supported}`; `WireProtocolTooNew` uses v2's simplified public sentence. Both stores' readers share the gate. Still host-only: a typed `WIRE_VERSION_TOO_LOW` code and its `details` reaching CLI/TUI diagnostic logs.
3. **Session/state protocol** — `1dd5578649`, `ebc2a9f960`, `2be062e8ea`: streamed block indexes/timing and `step.stream_reset`, live task state over WS, and bound-environment cwd for status/git/export. Touches `server/*`, `events/project.rs`, `session/*`. `011b1d4163` is `not-applicable` (all changed source lives in the retired kap-server; v1 is the only protocol axis).
4. **Goal and media** — `aca85b92fc`: requester-side cache-break baselines. Touches `llm/*`. `077239666a` is ported engine-side (PATCH goal route + objective action); its host-only revision gating / SDK wiring is recorded in the allowlist note. `b4da921509` and `131c21a35a` are `not-applicable` on source evidence: this fork has no `ReadMediaFile` tool or message-level tools, so neither hidden media results nor empty-content system messages with `tools` can exist.
5. **Host/plugin and steer** — `a5724dd32e`, `80193374eb`: `SteerResult` tracking across turns, plugin archive caching and MCP OAuth cancel/invalidate lifecycle.
6. **Host-only retirement** — `2a6d1cf570` is `ported`; any remaining `tracked` whose behavior lives exclusively in kap-server/klient must be judged `not-applicable` against this fork's single protocol axis, never ported as a second stack.

## Verdict vocabulary

`PORTED` = engine behavior is implemented and pinned by a test; `NA` =
not-applicable by architecture, with a reason; `TRACKED` = gap queued for a
future slice.

## Tool surface (v2 `src/agent/tools`)

The tool table is the one surface the model reads verbatim, so it was aligned
name by name against `D:/kimi/kimi-code` @ `7f6bd87ce8`, then description by
description, then field by field.

### Removed — never a v2 tool, or deleted upstream

| Tool | Evidence |
|---|---|
| `ListDirectory` | v2's `Read` says "Directories are not supported; use `ls` via Bash". The only listing helper v2 ever had was `tryNativeListDirectory`, deleted in `004903009c chore(agent-core): drop the dead native-tools wrappers`. The capability stays as `native::list_directory` for the environment prompt and the napi binding. |
| `SwitchSearchEngine` | Zero hits in v2's history; fork-original (`c9d2f5b00 feat: add multi-engine hot-switch for web search`). The host API `setSearchEngine` stays — it is the manual half the `/search` command drives. |
| `Team` | v2 removed it in `920add890d feat!: add hello-world mode and remove Team`, together with the `/team` slash command. |

`Lsp` also became `lsp` (v2 `readonly name = 'lsp'`), which exposed a live
inconsistency: `permission/mod.rs` listed both spellings, so a rule could miss
the tool depending on which list answered.

### Descriptions

Every description is now v2's `.md` rendered with the same placeholder values:
Bash, Glob, Edit, Grep, Write, Read, TaskList, TaskOutput, WaitFor,
AskUserQuestion, and Agent including its background variant. Where this engine
has behavior v2 has no reason to describe, the fork's paragraph is kept under a
heading that says so: Edit and Read carry the CRLF and encoding rules, Read the
sensitive-file and UTF-16 rules, WaitFor the steering interruption, Agent the
2-hour timeout.

### Schema fields

Checked field by field against v2's zod schemas: no missing parameter.
`TaskStop.reason`, `Read.column_offset` / `Read.max_chars`, `Glob.head_limit` /
`Glob.offset`, `Grep.multiline` / `Grep.type`, `Edit.replace_all` all exist.

### Open

- **PowerShell** (v2 `agent/tools/os/powershell`, 165 + 320 lines): the
  discovery, encoding, spawn-env and description-variant layers are ported
  (`native/powershell.rs`, 14 tests); the tool body and its dispatch are not.
  Reading `powershellTool.ts` end to end settled what the body needs:
  registration gated on `process.platform === 'win32'` (`:319`), a `ProcessTask`
  registered with `ShellToolResults`, `approvalRule:
  literalRulePattern(name, command)`, and `resolveExecutable` →
  `processService.spawn(exe.path, buildPowerShellArgs(script), {cwd, env})`.
- **Memory** (v2 `app/memory`, 4 files, 1055 lines): v2 exposes one `Memory`
  tool with seven actions (`search` / `read` / `write` / `list` / `delete` /
  `log` / `revert`); this engine exposes six `memory_*` tools and has neither
  `search`, `log` nor `revert`.

### Shell task lifecycle (v2 `agent/tools/os/bash/process-task.ts`, 318 lines —
read end to end)

Three behaviours of that file were checked against `tools/bash_exec.rs` and
`storage/task_runner.rs`:

| v2 behaviour | v2 line | This engine | Verdict |
|---|---|---|---|
| Settlement statuses `completed` / `failed` / `killed`, with `stopReason` on a non-abort failure | `:70-79` | `TaskStatus::{Completed,Failed,Killed}` (`task_runner.rs:146`) | aligned |
| Task id prefix `bash`, `command` + `pid` + `exitCode` + `parentToolCallId` in the task info | `:37`, `:97-106` | `generate_task_id("bash")`; `pid` / `exitCode` not carried | id aligned, the three fields are the same gap `backgroundStartedResult` records |
| Wait for the process, **then** drain the streams with a 250 ms grace | `:65-69`, `:33` | drains concurrently with `child.wait()`, bounded only by the task timeout | **gap**: a grandchild holding the pipe keeps the task running until the 600 s ceiling instead of settling at exit + 250 ms |
| A stream that closes without `end` is `ERR_STREAM_PREMATURE_CLOSE` | `:171-178` | `read_bounded_stream` treats a closed pipe as EOF | **not-applicable**: the distinction is a Node stream artifact; a tokio pipe reader reports EOF, and the fork has no `close`-without-`end` state to test |
| `SIGTERM` on abort, `SIGKILL` only on `forceStop` | `:56-58`, `:87-95` | `child.kill()` (SIGKILL on Unix) on timeout and on drop | **recorded divergence** — a killed shell cannot clean up its own children |

### Bash tool (v2 `agent/tools/os/bash/bashTool.ts`, 530 lines + `bash.ts`,
72 lines — both read end to end)

| v2 behaviour | v2 line | This engine | Verdict |
|---|---|---|---|
| `superRefine` rejects `timeout` above 300 s (foreground) / 86400 s (background) | `bash.ts:54-65` | schema carried `maximum: 9007199254740991` while the field description promised both caps | **fixed**: `anyOf` of the two bounded arms, pinned by `core_tool_defs` tests |
| Windows `>NUL` redirects are rewritten to `>/dev/null` before spawn, skipping quoted spans, heredocs, command substitutions, backticks and comments | `bashTool.ts:182`, `:317-530` | `tools/bash_null_redirect.rs` ports the shell-aware walk; `bash_with` applies it only under `cfg!(windows)` | **ported** — 11 tests, including a char-index walk so non-ASCII text survives (v2 walks UTF-16 units; a byte walk here corrupted Chinese) |
| Command is `cd <quoted-cwd> && <command>` rather than `Command::current_dir` | `:151-159` | `current_dir` on the child | recorded divergence: same observable cwd, different argv; not a defect |
| Tool display carries `language: 'bash'`, `cwd`, `command`, `description` | `:111-117` | `ToolInputDisplay::Command` already has `language`, and `acp/events_map_golden.rs:192` emits `"bash"` | **was mis-recorded**: no gap; the earlier claim came from grepping only the construction sites, not the type |
| `toolInputDisplay.ts` declares 14 variants including `spec_review` | `toolInputDisplay.ts:75-80` | the Rust enum had 13 and its header claimed 12 | **fixed**: `SpecReview { dir, documents, options }` added, `kind()` returns `spec_review`, and the golden wire-shape test now covers all 14 |
| `GIT_TERMINAL_PROMPT` inherits the process value, defaulting to `0` | `:155` | `bash_exec.rs` uses `shell_git_terminal_prompt()` on both spawn sites; the ACP reverse-terminal env keeps an explicit `0` because a client's process tree cannot inherit this engine's environment | **ported**, with the ACP boundary recorded in both comments |

## state/ (v2 `src/state` — 7 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| `defineState` / `ReplayableStateKey` / folds | `storage/state_store.rs` (`read_state`/`write_state`/`apply_write`, SQL rows) + session undo anchors | PARTIAL/NA | `storage::state_store::*`, `session::tests::undo_*` | Rust keeps durable participant state in one SQLite table and rolls back via checkpoint stacks; it does not carry v2's contribution/fold model or `RestorePhase` (v2 `state/RestorePhase` guards DI construction order). The durable store + undo behavior is reproduced; the DI contribution/fold machinery has no Rust analogue and is NA by architecture. |
| `foldEventStateContributions` (event/state registry) | `_` | NA | — | Rust has no event-state contribution registry; state is written explicitly through host callbacks / engine domains. Introducing one would re-create v2's DI layer, contrary to the engine seam design. |
| `UndoableProtocol` (append/compact/clear/undo) | `session` undo cut + `state_store` rollback | PORTED | `session-undo.test.ts`, `session::tests::undo_*` | v2 checkpoint/undo semantics reproduced; host and engine agree on depth. |

Next slice candidate: the v2 `runtime/` capability layer (`fs/process/watch/terminal` behind a status lifecycle) must get its own decision (port or ADR) before code changes.
## runtime/ (v2 `src/runtime` — 9 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| `Runtime` / `RuntimeLease` / `RuntimeProvider` / registry / watch / terminal capabilities | `_` (host-side concern) | NA | — | Rust engine has no capability runtime: filesystem/process/bash/grep/glob/read/write/edit run in-process via native tools, and watch is intentionally `/reload` (AGENTS B-27, `notify` not available). Adding a `fs/process/watch/terminal` capability layer would duplicate those primitives and is not in the engine seam. |

## wire/ (v2 `src/wire` — 12 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| `wire/record`, RPC wire, REST/WS schemas | `rpc/types.rs`, `server/ws_protocol.rs`, native+acp transport | PORTED (protocol) | `check:parity`, napi integration | Rust's wire contract is exercised by `check:parity`. One recorded host-visible gap remains: ACP projection drops `tool.result.note`. |
## tool/ (v2 `src/tool` — 10 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Tool execution / scheduling / dedup / permissions | `tools/dispatch.rs`, `tools/mod.rs`, `tools/tool_dedupe.rs`, `tools/select_tools.rs`, `turn_loop/tool_scheduler.rs`, `permission/mod.rs` | PORTED | `tools/tests.rs`, `turn_loop/run_turn/tests/*` | Concurrent scheduling, dedup/repeat-breaker, tool answer refusal (args parse failure) ported; `select_tools` deferred MCP integration present. |

## mcpCore/ (v2 `src/mcpCore` — 16 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Connection manager, transport, OAuth, deferred disclosure | `mcp/manager.rs`, `mcp/client*.rs`, `mcp/http.rs`, `napi_bindings/mcp.rs`, node-sdk mcp-* | PORTED | `mcp/manager/tests.rs`, `napi-integration.test.ts` | Explicit on-demand reconnect entry points (`reconnect`, `reconnect_after_current`) present; no startup retry scheduler (recorded difference: needs `McpError` kind + Arc scheduler). |

## workspace/ (v2 `src/workspace` — 54 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Workspace service, trust, default workspace, folder status | `workspace_trust.rs`, `default_workspace.rs`, `server/fs_routes.rs`, `workspace_trust_disclosure.rs` | PORTED | `server/tests/*` (folder/git status, trust disclosure) | Folder-scoped git status + batch/pull-request routes, symlink-aware fs listing, default workspace route, trust disclosure present. |

## session/ (v2 `src/session` — 76 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Session handle, replay, restore, fork, agents sub-sessions | `session/mod.rs`, `session/tests.rs`, `session/sqlite_store.rs`, `subagent/manager.rs`, `events/project.rs` | PARTIAL/PORTED | `session/tests.rs`, `subagent/manager/tests.rs` | Sqlite resume/restore, swarm members after restore, fork/background resume covered; some v2 restore-phase semantics are NA (Rust sessions usable only once handle exists). |

## persistence/ (v2 `src/persistence` — 13 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Sqlite/KV store, WAL/snapshot | `session/sqlite_store.rs`, `minidb` (TS), `storage/state_store.rs` | PORTED (Rust side) | `session/sqlite_store/tests.rs`, `storage/state_store` tests | Rust persistence is SQLite-backed state store; bun/TS side uses minidb separately (not this engine crate). |

## hooks.ts (v2 `src/hooks.ts` — 1 file)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| External hook events and guard dispatch | `tools/external_hooks.rs`, `pipeline/mod.rs`, `callbacks` | PORTED | `tools/external_hooks` tests | Hook result payload now carries `agentId`/`turnId`; 12 events ported. |

## agent/ (v2 `src/agent` — 274 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Turn loop, tool execution, compaction (micro/full), injection, goal, permission, profile, prompt, shell, state, task, token counting, tool dedup/dedupe/activation/disclosure, mode mutex, stale guard, swarm/tower/btw/skill/plan reminders | `turn_loop/*`, `tools/*`, `compaction/*`, `goal/*`, `permission/*`, `prompt/*`, `injection/*`, `session/*`, `storage/*`, `subagent/*`, `swarm/*` | PORTED (broad, with recorded gaps) | `turn_loop/run_turn/tests/*`, `tools/tests.rs`, `compaction/tests.rs`, `goal/*`, `permission/*` | Major v2 engine behaviors ported; a few recorded divergences remain (e.g. task-runner startup retry scheduler = TRACKED; some reminder/steering details under `injection/` / `stepRetry`). |

## features/ (v2 `src/features` — 253 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| astronaut/inspection, date-change, debug events, external hooks, file history, goal, interaction, plan, reminder, skill, spill, stale guard, swarm, todo, token counting, tower, usage | Same counterparts as `agent/` plus `features`-specific tool modules (`tools/*`, `injection/*`) | PORTED (majority) | `tools/tests.rs`, `external_hooks`, `goal`, `swarm`, `tower` | Feature-level UX/business flows are split across Rust engine + TS host; engine half largely present. Visual/web-specific bits stay in TypeScript (that is the intended boundary). |

## os/ (v2 `src/os` — 17 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Host env/fs/process/terminal abstractions | Rust `native/*`, `kaos/*` (TS), napi host | PARTIAL/NA | `native/*`, kaos tests | Rust drives local execution directly via `kaos` + native modules rather than the v2 host-service abstraction tree; host environment probing remains a host concern. |

## program/ (v2 `src/program` — 2 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|--- |
| CLI entry | `main.rs`, `repl/mod.rs`, `acp/mod.rs`, server entry | PORTED | smoke tests / `main` integration | Standalone CLI + stdio + ACP + server entries all present. |

## _base/ (v2 `src/_base` — 68 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| DI scope, collections, types, TS infra | Rust type system / no DI container | NA | — | Rust uses module/struct composition, not v2's DI container; not a behavior gap. |

## debug/ (v2 `src/debug` — 9 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Debug surface | `server` debug RPC, `vis` (separate app) | PARTIAL | `server` tests | Some web-state inspect via kimi-inspect; engine internals (sessions, tools) exposed. |

## errors.ts / hooks.ts / index.ts

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Errors, hooks entry, index | `errors.rs`/`anyhow`, `tools/external_hooks.rs`, `lib.rs` | PORTED (paradigm difference) | clippy/tests | Rust uses `anyhow`/`thiserror`-style errors; behaviorally covered. |

## kosong/ (v2 `src/kosong` — 62 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|--- |
| LLM wire transport/client | `kosong` (TS package) + Rust `llm/*` | PORTED (split) | `llm/*` tests, kosong tests | Rust engine has native HTTP/WS LLM transports; TS `kosong` remains the host/protocol contract package; both sides tested. |

## program/ (v2 `src/program` — 2 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| Program container (workspace/collaboration services orchestrator) | Rust engine entrypoints `main.rs` / `repl/mod.rs` / `acp/mod.rs` / `server/*` + `pipeline/mod.rs` composition | PARTIAL/NA-as-host | server/acp/repl smoke tests | The engine side does not host v2's DI program container; composition is direct in Rust. Host-facing workspace/agent profile/MCP/instruction layers remain TypeScript concerns by boundary. |

## _base/ (v2 `src/_base` — 68 files)

| v2 surface | Rust counterpart | Status | Tests | Notes |
|---|---|---|---|---|
| DI scope/collections/events, TS infra | Rust modules as direct types/functions | NA | — | Rust does not emulate v2's DI container; parity is measured at behavior surfaces (tools, session, wire), not infra plumbing. |

## File-level spot checks (v2 `src/tool/*`)

- `args-validator.ts`: v2 compiles one Ajv validator per tool and refuses a
  non-conforming call with `Invalid args for tool "<name>": <detail>`, every
  error joined by `; `. Ported in `tools/args_validate.rs` as a **guarded
  subset**: draft 07/2019/2020 selection is irrelevant here because the engine
  reads one schema at a time and understands the keywords it implements; a
  keyword it does not implement is ignored, matching Ajv under `strict: false`.
  The subset cannot silently rot:
  `every_keyword_the_engines_own_tools_use_is_implemented` walks every built-in
  tool schema and fails on a keyword that is neither implemented nor an
  annotation — `format` is deliberately in neither list, because Ajv does
  enforce it through `addFormats`. Recorded differences: `anyOf`/`oneOf`/`allOf`
  report the branch verdict instead of Ajv's nested per-branch trace, and an
  unresolvable external `$ref` is skipped rather than rejected (the engine
  cannot fetch it). Wired at the same gate as v2, after the swarm guard:
  `turn_loop/run_turn.rs::unusable_tool_arguments`, pinned end to end by
  `a_schema_violating_call_is_refused_before_the_tool_runs`.
- `input-schema.ts`: host/`kosong` side (zod→draft-7), Rust only consumes `input_schema` from wire.
- `native-glob-match.ts`: Rust native glob matcher exists (`grep_types`/glob logic), behavior parity wired behind Rust tools.
- `output-accumulator.ts`: Rust `tool_result_truncation.rs::ToolResultTruncator` maps the retained/total budget semantics; matched by tests.
- `path-access.ts`: ported `native/path_access.rs` + `realpath_access.rs` (lexical + exact-symlink gate).
- `result-builder.ts`: model-facing markers ported `render_status`; native truncation is Rust-side.
- `rule-match.ts`: permission rule path glob matching lives with `permission/mod.rs`; follow-up to audit patterns against Rust matcher.
- `tool-args-parse.ts`: ported `llm/tool_args.rs`, tests cover absent/empty/valid/invalid.
- `toolContract.ts`: fields mirrored into Rust `ExecutableToolResult`, napi/ACP structs.
- `toolInputDisplay.ts`: projected into Rust tool display (`tools/dispatch`/native); UI metadata carries through.

## File-level spot checks (v2 `src/wire/*`)

- `wire/errors.ts`: error classes for wire records; Rust side uses `anyhow`/result propagation and storage error mapping; behavior parity at record append/read sites.
- `wire/migration/v1.1.ts` … `v1.6.ts`: schema migrations for journal format. Rust uses SQLite schema (state_store), with separate migration modules/tests (`session/sqlite_store` migration paths); journal-style replay migration is NA because there is no JSONL journal/torn-tail repair path.
- `wire/record.ts`, `wire/repair.ts`: v2 record/journal repair. Rust SQLite rows are transactional; repair path NA (a torn row cannot exist in transactional store).
- `wire/wire.ts`, `wire/wireService.ts`: wire record encode/decode and publish; Rust `rpc/types.rs`, `events/types.rs`, `server/ws_protocol.rs`, `events/project.rs` carry the equivalent event/record contracts; exercised by `check:parity` and napi tests.

## File-level spot checks (v2 `src/os/*`)

- `os/backends/host/hostClockService.ts`: Rust uses `std::time` / `chrono` in process; host clock is native.
- `hostEnvironmentService.ts`: Rust relies on host-provided config (OS platform handled via kaos/native shell path); environment factoring is host-side.
- `hostFsService.ts`, `hostFsErrors.ts`, `hostFileSystem.ts`: Rust file access via native tools + kaos path; guards in `native/path_access.rs` + `realpath_access.rs`, content I/O in Rust tools/native.
- `hostFsWatchService.ts`, `hostFsWatch.ts`: NA watch (B-27, `/reload` by design, notify not available).
- `hostProcessService.ts`, `hostProcess.ts`: Rust `tools/bash_exec.rs` + kaos shell abstraction; no host-process service (native in-process execution).
- `hostTerminalService.ts`, `terminal*.ts`: TUI is textual-rs/crossterm in separate host crates; engine does not own terminal.
- `tools/rgLocator.ts`, `tools/runRg.ts`: Rust `tools/grep_types.rs` and grep tool native implementation replace host rg invocation.
- `configSection.ts`: configuration provider for host OS service; Rust config handled in `config/mod.rs`.

## File-level spot checks (v2 `src/debug/*`)

- `debug/debugCascade*.ts`: N/A (debug graph of DI cascade; Rust has no DI container).
- `debug/debugGraph*.ts`: N/A (DI graph inspection; Rust module graph handled by scripts/rust-module-deps baseline).
- `debug/debugLedger*.ts`: NA (DI linking debug).
- `debug/errors.ts`: Rust `thiserror`-style errors in tools; behavior parity via tests rather than same file.
- `debug/scopeTree.ts`: NA (DI scope debugging).

## File-level spot checks (v2 `src/mcpCore/*`)

- `mcpCore/client-http.ts` / `client-remote.ts` / `client-sse.ts` / `client-stdio.ts` / `client-shared.ts`: Rust `mcp/http.rs` + `mcp/client*.rs` + native HTTP(SSE); transport parity tested in `mcp/manager/tests.rs` / napi integration.
- `mcpCore/config-schema.ts`, `configView.ts`: validated by `node-sdk` config types and server config; Rust reads through config structs; behavior at runtime parity (tests/napi config).
- `mcpCore/connection-manager.ts`: Rust `mcp/manager.rs` (explicit reconnect/reconnect_after_current; no startup retry scheduler = recorded difference/Gap).
- `mcpCore/errors.ts`: Rust `mcp/errors.rs`.
- `mcpCore/oauth/*`: Rust `mcp/oauth` + `packages/oauth/*` + napi bindings; host-side OAuth flow supported.
- `mcpCore/tool-naming.ts`: Rust `native/tool_naming.rs` (per-server tool names).
- `mcpCore/types.ts`: Rust `mcp/mod.rs`, `rpc/types.rs`, `tools/mod.rs`.

## File-level coverage: remaining modules (automated from v2ref)

> Status/source assessments are module-level verdicts; each row maps to the corresponding Rust side at the module boundary. `tests` cite the closest test anchor; a missing deep per-file note is recorded as `TRACKED`.

### `_base/` (68 files) — NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `_base/asyncEventQueue.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/contribution/registry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/cascadeEngine.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/collection.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/dependencyGraph.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/descriptors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/fiber.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/graph.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/instantiation.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/instantiationService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/lifecycle.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/scope.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/scopeUnits.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/service.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/serviceCollection.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/test.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/testInstantiationService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/util/linkedList.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/codes.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/errorMessage.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/serialize.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/unexpectedError.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/event.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/bufferedReadable.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/decodeText.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/environmentProbe.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/globPattern.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/loginShellPath.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/shellPathBridge.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/disposer.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/index.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/keyedResource.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/ledger.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/lifecycleMachine.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/fileLog.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/formatter.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/log.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/logConfig.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/logService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/native-tools.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/session/hooks/types.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/state/stateRegistry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/text/encoding.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/text/frontmatter.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/text/line-endings.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/abort.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/canonical-args.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/env.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/fileMeta.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/fs.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/hero-slug.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/isoDateTime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/paths.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/private-address.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/promise.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/proxy.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/render-prompt.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/retry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/timer.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/typeEquality.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/types.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/undici-npm.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/workdir-slug.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/xml-escape.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/version.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |

### `agent/` (274 files) — PORTED (broad)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `agent/activityView/activityView.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/activityView/activityViewService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/actorService/agentActorService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentContext/agentContext.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentContext/agentSpace.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentsMdReminder/agentsMdReminder.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentsMdReminder/agentsMdReminderService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentsMdReminder/bashTargets.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/blob/agentBlobService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/blob/agentBlobServiceImpl.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/blob/byteLruCache.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/checkpoint/checkpointService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/command/agentCommand.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/command/agentCommandService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/command/commandContribution.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/compaction-summary-prefix.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/compactionHandoff.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextMemory.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextMemoryService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextTranscript.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/conversationTime.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/conversationUndoParticipants.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/loopEventFold.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/messageId.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/openToolExchange.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/toolResultRender.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/vacuousContent.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/contextProjector.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/contextProjectorService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/mediaProjection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/projection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/compaction-instruction.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/compactionOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/compactionUtils.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/fullCompaction.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/fullCompactionService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/strategy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/goal/judge/goalJudgeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/goal/judge/judgeAgentProfile.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/goal/judge/judgePrompt.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/guardian/guardianService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/interruptionReminder/interruptionReminder.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/interruptionReminder/interruptionReminderOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/interruptionReminder/interruptionReminderService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/llmRequester.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/llmRequesterService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/llmRequestOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/toolCallIdNormalizer.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/engineOverride.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loop.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loopContinuation.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loopContinuationService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loopService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/nativeBackgroundAgentTask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/stepRequest.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/stepRequestQueue.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/turnEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/turnOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcp.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcpDiscoveryOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcpEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcpService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/output.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/tools/auth.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/tools/mcp.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/file-type.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/image-compress.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/image-format-policy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/image-originals.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/imageConfigBridge.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/kimiFileUrl.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaReadContext.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaRef.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaResolver.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaResolverService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/promptMediaIntake.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/sessionMediaStore.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/sessionMediaStoreService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/videoUpload.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/webp-animated.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/webp-dec-wasm.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/webp-decode.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/flag.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/microCompaction.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/microCompactionOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/microCompactionService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/modeMutex/modeMutex.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/modeMutex/modeMutexService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionGate/permissionGate.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionGate/permissionGateService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/injection/permission-mode-auto-enter-reminder.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/injection/permission-mode-auto-exit-reminder.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/injection/permissionModeInjection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/permissionMode.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/permissionModeOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/permissionModeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/permissionPolicy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/permissionPolicyService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/auto-mode-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/auto-mode-ask-user-question-deny.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/dangerous-command-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/default-tool-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/fallback-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/git-control-path-access-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/git-cwd-write-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/guardian-review.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/path-utils.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/sensitive-file-access-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/session-approval-history.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-allow.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-deny.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-rule.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/yolo-mode-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/matchesRule.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/permissionRules.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/permissionRulesOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/permissionRulesService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/agentPlugin.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/agentPluginOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/agentPluginService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/index.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/pluginCommand/pluginCommand.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/pluginCommand/pluginCommandService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/context.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/profile.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/profileOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/profileService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/prompt.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptMetadataText.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptStepRequests.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/replayBuilder/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/agentRuntime.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/runtimeBinding.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/runtimeBindingOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/runtimeBindingService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/scopeContext/scopeContext.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/shellCommand/shellCommand.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/shellCommand/shellCommandService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/state/agentState.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/state/agentStateService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/stepRetry/stepRetry.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/stepRetry/stepRetryService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/notificationXml.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/persist.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/printDefaults.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/taskOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/taskService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/tools/format.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tokenCounting/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tokenCounting/tokenCounting.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tokenCounting/tokenCountingOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolActivation/toolActivation.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolActivation/toolActivationService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolApproval/toolApproval.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolApproval/toolApprovalService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolDedupe/toolDedupe.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolDedupe/toolDedupeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolDisclosure/toolDisclosure.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/beforeToolExecuteEvent.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolExecutor.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolExecutorEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolExecutorService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolHooks.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolScheduler.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/evaluate.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/toolPolicy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/toolPolicyService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/builtinToolAssemblyService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/toolContribution.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/toolRegistry.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/toolRegistryService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolResultTruncation/toolResultTruncation.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolResultTruncation/toolResultTruncationService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent-background-disabled.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent-background-enabled.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent-fork.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agentTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/subagent-task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/ask-user-question.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/ask-user.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/askUserQuestionTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/question-background-task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/edit/edit.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/edit/edit.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/edit/editTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/fetch-url/fetch-url.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/fetch-url/fetch-url.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/fetch-url/fetchUrlTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/envOverlay.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/github-request.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/github-tools.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/github.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/mainAgentOnly.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/bash.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/bash.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/bashTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/native-bash-process.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/process-task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/glob/glob.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/glob/glob.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/glob/globTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/grep/grep.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/grep/grep.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/grep/grepTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/read/read.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/read/read.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/read/readTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/write/write.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/write/write.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/write/writeTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/read-media-file/execute-media-read.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/read-media-file/read-media-file.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/select-tools/select-tools.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/select-tools/selectToolsTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-list/task-list.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-list/task-list.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-list/taskListTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-output/task-output.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-output/task-output.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-output/taskOutputTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-stop/task-stop.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-stop/task-stop.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-stop/taskStopTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/flag.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/task-wait.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/task-wait.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/taskWaitTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/web-search/web-search.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/web-search/web-search.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/web-search/webSearchTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/dynamicTools.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/flag.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelect.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectAnnouncements.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectAnnouncementsService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectSchemas.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectSchemasService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/undo/undo.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/undo/undoService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/cacheProbe.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/cacheProbeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/usage.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/usageEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/usageOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/userTool/userTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/userTool/userToolOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/userTool/userToolService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |

### `app/` (215 files) — PARTIAL/NA-as-host

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `app/agentIdentity/agentIdentity.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentIdentity/agentIdentityService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentIdentity/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileCatalog.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileContribution.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileRegistry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileRegistryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/builtinAgentProfileLoader.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/builtinAgentProfileLoaderService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/contribution.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/model-adaptations/claude-3.5-sonnet.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/model-adaptations/deepseek-v3.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/model-adaptations/gpt-4o.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/profile-shared.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/promptPrefix.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/system.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/auth.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/authService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/oauthProtocol.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/baidu/baidu.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/baidu/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/baidu/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/bing/bing.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/bing/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/bing/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/brave/brave.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/brave/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/csdn/csdn.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/csdn/fetchCsdnArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/csdn/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/duckduckgo/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/duckduckgo/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/duckduckgo/searchDuckDuckGo.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/engine-html.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/engine-http.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/engine-undici.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/exa/exa.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/exa/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/github/github.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/github/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/http3.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/juejin/fetchJuejinArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/juejin/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/juejin/juejin.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/linuxdo/fetchLinuxDoArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/linuxdo/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/linuxdo/linuxdo.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/sogou/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/sogou/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/sogou/sogou.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/startpage/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/startpage/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/startpage/startpage.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/web/fetchWebContent.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/web/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/zhihu/fetchZhihuArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/zhihu/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/zhihu/zhihu.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/providers/local-web-search.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/providers/moonshot-web-search.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/webSearch.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/webSearchService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/authLegacy/authLegacy.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/authLegacy/authLegacyService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bashParser/bashParser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bashParser/bashParserService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bootstrap/bootstrap.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bootstrap/bootstrapService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/capability.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/capabilityEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/capabilityService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/entries/context.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/entries/kimiCu.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/entries/kimiWebbridge.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/host.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/config.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configOverlayContributions.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configPure.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configSectionContributions.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/deprecations.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/migrations.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/sectionDiff.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/toml.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/tomlWriteback.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/editService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/fileEdit.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/fileEditService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/textModel.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/event.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/event2.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/eventBus.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/eventBusService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/eventService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/fiberEventResolver.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/feature/featureManager.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/feature/featureManagerService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/feature/featureServiceContribution.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/file/fileService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/file/fileServiceImpl.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flag.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flagRegistry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flagRegistryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flagService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/gateway/gateway.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/gateway/gatewayService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/git.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/gitParsers.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/gitService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/workTree.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/hostFolderBrowser/hostFolderBrowser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/hostFolderBrowser/hostFolderBrowserService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/builtInModelsDev.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/discovery.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/discoveryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/envOverlay.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/hostRequestHeadersAdapter.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/kosongConfig.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/kosongConfigService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDev.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDevImport.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDevImportService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDevUpstream.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/oauthTokenAdapter.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/llmProtocol/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/llmProtocol/message.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/llmProtocol/usage.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/configLoader.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/configStore.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/oauthCoordinator.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/oauthService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/oauthStore.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpManagement/mcpManagement.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpManagement/mcpManagementService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpRegistry/mcpRegistry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpRegistry/mcpRegistryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/archive.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/commands.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/github-resolver.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/manager.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/manifest.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/marketplace.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/plugin.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/pluginEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/pluginService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/source.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/store.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/projectLocalConfig/projectLocalConfig.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/remoteControl/flag.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/scopes.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/file-source.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/manifest.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/sessionExport.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/sessionExportService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/wire-scan.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/zip.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndex.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexMirrorService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexModel.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexProjector.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexSource.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionLegacy/sessionLegacy.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionLegacy/sessionLegacyService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionLegacy/sessionProtocol.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionManager/sessionLookup.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionManager/sessionManager.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionManager/sessionManagerService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/state/appState.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/state/appStateService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/task/task.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/task/taskService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/agentTelemetryContext.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/agentTelemetryContextService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/cloudAppender.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/cloudTransport.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/consoleAppender.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/coreVersion.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/events.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/privacy.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/telemetry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/telemetryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/providers/local-fetch-url.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/providers/moonshot-fetch-url.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/tools/fetch-url-types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/web.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/webService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/fileWorkspacePersistence.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspace.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspaceAlias.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspaceEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspacePersistence.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspaceService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceAliases/workspaceAliases.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceAliases/workspaceAliasesService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceLifecycle/workspaceLifecycle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceLifecycle/workspaceLifecycleService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceSessions/workspaceSessions.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceSessions/workspaceSessionsService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |

### `debug/` (9 files) — NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `debug/debugCascade.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugCascadeService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugGraph.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugGraphService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugLedger.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugLedgerService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/index.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/scopeTree.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |

### `features/` (253 files) — PORTED (majority)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `features/astron/flag.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/btw/btw.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/btw/btwFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/btw/btwService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronTask.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/errors.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/clock.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/cron-expr.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/format.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/jitter.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-create/cron-create.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-create/cron-create.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-create/cronCreateTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-delete/cron-delete.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-delete/cron-delete.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-delete/cronDeleteTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-list/cron-list.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-list/cron-list.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-list/cronListTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/dateChange.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/dateChangeFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/dateChangeService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/disclosureBaseline.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/debugEvents/debugEvents.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/debugEvents/debugEventsFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/debugEvents/debugEventsService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/agent/agentExternalHooks.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/agent/agentExternalHooksService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/app/externalHooksRunner.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/app/externalHooksRunnerService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/externalHooksFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/matchHooks.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/runHook.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/userPrompt.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/session/sessionExternalHooks.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/session/sessionExternalHooksService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/feature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/featureAssembly.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/featureAssemblyService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/featureRegistry.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistory.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryRetention.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/flag.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/errors.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalDeadlineScheduler.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalDeadlineSchedulerService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-active-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-blocked-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-budget-limited-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-paused-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goalInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/create-goal/create-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/create-goal/create-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/create-goal/createGoalTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/get-goal/get-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/get-goal/get-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/get-goal/getGoalTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/outcome-prompts.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/serialize.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/set-goal-budget/set-goal-budget.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/set-goal-budget/set-goal-budget.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/set-goal-budget/setGoalBudgetTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/update-goal/update-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/update-goal/update-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/update-goal/updateGoalTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interaction.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interactionFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interactionOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interactionService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/sessionInteractions.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/exitPlanModeReview.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-exit-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-full-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-inline-full-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-inline-reentry-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-inline-sparse-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-reentry-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-sparse-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/planModeInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/parsePlanToTodos.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/plan.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planToTodoConverter.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/profile/plan.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/enter-plan-mode/enter-plan-mode.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/enter-plan-mode/enter-plan-mode.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/enter-plan-mode/enterPlanModeTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/exit-plan-mode/exit-plan-mode.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/exit-plan-mode/exit-plan-mode.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/exit-plan-mode/exitPlanModeTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/reminderFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/reminderService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/systemReminder.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/profile/init.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/profile/init.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/sessionInit.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/sessionInitFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/sessionInitService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/builtin.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/check-kimi-code-docs.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/check-kimi-code-docs.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/custom-theme.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/custom-theme.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/import-from-cc-codex.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/import-from-cc-codex.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/mcp-config.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/mcp-config.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/registry.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill/consolidate/SKILL.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill/review/SKILL.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill/SKILL.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/update-config.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/update-config.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/write-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/write-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtinSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/errors.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/fileSkillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/inMemorySkillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/parser.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/registry.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/skillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/skillRoots.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/skillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/userFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/prompt.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/session/skillCatalog.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/session/skillCatalogData.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/session/skillCatalogService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skillFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skillOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skillService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/tools/skill.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/tools/skill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/tools/skillTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/explicitFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/extraFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/pluginSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/rootFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/runtimeSkillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/workspaceSkillCatalog.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/workspaceSkillCatalogService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spillFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spillService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spillStore.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuard.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuardFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuardOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuardService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/enter-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/exit-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/injection/swarmInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/swarm.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/swarmService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/session/agentRunBatch.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/session/sessionSwarm.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/session/sessionSwarmService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/swarmFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/swarmOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agent-swarm-fork.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agent-swarm.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agent-swarm.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agentSwarmTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoItem.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoListReminder.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todo-list-write-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todo-list.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todo-list.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todoListTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tokenCounting/tokenCountingFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/flag.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/tower-mode-exit-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/tower-mode-full-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/tower-mode-sparse-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/towerModeInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/baseWip.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/frontmatter.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/git.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/index.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/paths.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/repoRoot.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/store.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/finding/finding.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/finding/finding.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/finding/findingTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/inbox/inbox.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/inbox/inbox.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/inbox/inboxTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/init/init.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/init/init.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/init/initTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/merge/merge.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/merge/merge.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/merge/mergeTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/mission/mission.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/mission/mission.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/mission/missionTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/plan/plan.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/plan/plan.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/plan/planTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/review/review.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/review/review.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/review/reviewTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/send/send.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/send/send.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/send/sendTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/spawn/spawn.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/spawn/spawn.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/spawn/spawnTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/status/status.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/status/status.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/status/statusTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/support.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/teardown/teardown.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/teardown/teardown.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/teardown/teardownTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tower-worker-overlay.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tower.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerRateLimit.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerRateLimitService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/workerProfile.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/usage/usageFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |

### `kosong/` (62 files) — PORTED (split)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `kosong/contract/capability.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/generate.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/inspection.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/message.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/messageHelpers.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/provider.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/requestTrace.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/tokens.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/tool.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/usage.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/catalog.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/catalogService.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/completionBudget.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/hostRequestHeaders.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/inspection.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/model.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/model.types.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelAuth.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelOAuth.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelRequester.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelRequesterImpl.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelService.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/thinking.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/protocol.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/protocolBase.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/protocolTrait.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropic-profile.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropic.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropic.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropicHooks.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/antigravity/antigravity.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/antigravity/antigravity.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/antigravity/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/google-genai/google-genai.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/google-genai/google-genai.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/google-genai/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/merge-user-messages.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/chat-completions-stream.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-common.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-legacy.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-legacy.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-responses.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-responses.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openaiHooks.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/reasoning-key.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/request-auth.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/tool-call-id.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/protocolAdapterRegistry.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/provider.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providerDefinition.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi-errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi-files.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi-schema.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/standard.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providerService.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/recordDiff.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |

### `mcpCore/` (16 files) — PORTED (gap: startup retry scheduler)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `mcpCore/client-http.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-remote.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-shared.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-sse.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-stdio.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/config-schema.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/configView.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/connection-manager.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/errors.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/callback-server.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/coordinator.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/provider.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/service.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/store.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/tool-naming.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/types.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |

### `os/` (17 files) — PARTIAL/NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `os/backends/host/hostClockService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostEnvironmentService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostFsService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostFsWatchService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostProcessService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostTerminalService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/tools/rgLocator.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/tools/runRg.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/configSection.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostClock.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostEnvironment.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostFileSystem.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostFsErrors.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostFsWatch.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostProcess.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/terminal.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/terminalErrors.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |

### `persistence/` (13 files) — PORTED (Rust)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `persistence/backends/memory/inMemoryStorageService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/minidb/flag.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/minidb/miniDbQueryStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/appendLogStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/atomicDocumentStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/blobStoreService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/fileStorageService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/projectLocalConfigService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/appendLogStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/atomicDocumentStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/blobStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/queryStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/storage.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |

### `program/` (2 files) — PARTIAL/NA-as-host

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `program/program.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `program/programDependencies.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |

### `runtime/` (9 files) — NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `runtime/fakeRuntime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/localRuntime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimePath.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeProvider.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeRegistry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeUnitHost.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeWorkspaceView.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/standaloneRuntime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |

### `session/` (76 files) — PORTED/PARTIAL

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `session/agentLifecycle/agentLifecycle.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/agentLifecycleService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/errors.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/mainAgent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/managedAgent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/explore-overlay.md` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/gitContext.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/profiles.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/summary-continuation.md` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/subagentMetadata.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/approval/approval.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/approval/approvalService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/errors.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/mcp/ephemeralMcpServers.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/mcp/mergedConnectionView.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/mcp/sessionMcpHandle.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/process/processRunner.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/process/processRunnerService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/question/question.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/question/questionService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionActivity.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionActivityService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionOutcomeMirror.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionOutcomeMirrorService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionAgentProfileCatalog/agentProfileCatalogSeed.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionAgentProfileCatalog/sessionAgentProfileCatalog.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionAgentProfileCatalog/sessionAgentProfileCatalogService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionContext/sessionContext.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionInstructions/instructionsProvider.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionLog/sessionLogService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/promptMetadata.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/sessionMetadata.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/sessionMetadataService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/sessionMetaEvents.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/agentTitlePromptSource.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/agentTitlePromptSourceService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/flag.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/sessionTitle.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/sessionTitleService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicy/sessionToolPolicy.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicy/sessionToolPolicyService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicyGate/sessionToolPolicyGate.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicyGate/sessionToolPolicyGateService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/state/sessionState.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/state/sessionStateService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/acpBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/backendCompat.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/claudeCodeBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/codexBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/codexWire.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/configSection.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/flag.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/subagentBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/subagentBackendService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/configSection.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/flag.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/forkCompat.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/mirrorAgentRun.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/persistentSubagent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/persistentSubagentService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/runAgentTurn.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/spawn.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagentModelsValidation.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagentModelsValidationService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagentService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/terminal/terminalService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/tokenCounting/sessionTokenCounting.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/tokenCounting/sessionTokenCountingService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/tokenCounting/tokenCountingAgentModel.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/usage/sessionUsage.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/usage/sessionUsageService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/usage/usageAgentModel.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/workspaceContext/workspaceContext.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/workspaceContext/workspaceContextService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/workspaceInfo/workspaceInfo.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |

### `state/` (7 files) — PARTIAL/NA (DI)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `state/agentEffect.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/agentModel.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/errors.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/eventDispatcher.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/eventDispatcherService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/state.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/stateContribution.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |

### `tool/` (10 files) — PORTED (guarded subset: `tools/args_validate.rs`)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `tool/args-validator.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/input-schema.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/native-glob-match.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/output-accumulator.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/path-access.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/result-builder.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/rule-match.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/tool-args-parse.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/toolContract.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/toolInputDisplay.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |

### `wire/` (12 files) — PARTIAL/NA journal model

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `wire/errors.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/migration.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.1.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.2.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.3.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.4.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.5.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.6.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/record.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/repair.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/wire.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/wireService.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |

### `workspace/` (54 files) — PORTED

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `workspace/sandbox/sandbox.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/coldSessionArchive.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/internal/addressing.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/internal/forkTurnSlice.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/sessionLifecycle.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/sessionLifecycleEvents.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/sessionLifecycleService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/state/workspaceState.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/state/workspaceStateService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/configSection.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/explicitAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/explicitAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/extraAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/extraAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentFile.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentFileDiscovery.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentProfileFromFile.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentRoots.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/paths.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/systemFile.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/types.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/pluginAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/pluginAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/userAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/userAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/workspaceAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/workspaceAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceContext/workspaceContext.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceDirs/workspaceDirs.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceDirs/workspaceDirsService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fs.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fsService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fsWatch.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fsWatchService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/errors.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/fsProcess.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/fsSearch.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/rgLocator.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/runRg.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceGit/workspaceGit.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceGit/workspaceGitService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstance/workspaceInstance.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstance/workspaceInstanceManager.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstance/workspaceInstanceManagerService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstructions/workspaceInstructions.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstructions/workspaceInstructionsService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcp/workspaceMcp.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcp/workspaceMcpService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcpConfig/workspaceMcpConfig.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcpConfig/workspaceMcpConfigService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceTrust/trustRecord.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceTrust/workspaceTrust.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceTrust/workspaceTrustService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |


## File-level coverage: remaining modules (automated from v2ref)

> Status/source assessments are module-level verdicts; each row maps to the corresponding Rust side at the module boundary. `tests` cite the closest test anchor; a missing deep per-file note is recorded as `TRACKED`.

### `_base/` (68 files) — NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `_base/asyncEventQueue.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/contribution/registry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/cascadeEngine.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/collection.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/dependencyGraph.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/descriptors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/fiber.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/graph.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/instantiation.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/instantiationService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/lifecycle.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/scope.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/scopeUnits.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/service.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/serviceCollection.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/test.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/testInstantiationService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/di/util/linkedList.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/codes.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/errorMessage.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/serialize.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/errors/unexpectedError.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/event.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/bufferedReadable.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/decodeText.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/environmentProbe.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/globPattern.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/loginShellPath.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/execEnv/shellPathBridge.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/disposer.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/index.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/keyedResource.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/ledger.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/lifecycle/lifecycleMachine.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/fileLog.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/formatter.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/log.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/logConfig.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/log/logService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/native-tools.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/session/hooks/types.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/state/stateRegistry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/text/encoding.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/text/frontmatter.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/text/line-endings.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/abort.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/canonical-args.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/env.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/fileMeta.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/fs.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/hero-slug.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/isoDateTime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/paths.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/private-address.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/promise.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/proxy.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/render-prompt.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/retry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/timer.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/typeEquality.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/types.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/undici-npm.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/workdir-slug.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/utils/xml-escape.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `_base/version.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |

### `agent/` (274 files) — PORTED (broad)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `agent/activityView/activityView.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/activityView/activityViewService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/actorService/agentActorService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentContext/agentContext.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentContext/agentSpace.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentsMdReminder/agentsMdReminder.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentsMdReminder/agentsMdReminderService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/agentsMdReminder/bashTargets.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/blob/agentBlobService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/blob/agentBlobServiceImpl.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/blob/byteLruCache.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/checkpoint/checkpointService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/command/agentCommand.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/command/agentCommandService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/command/commandContribution.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/compaction-summary-prefix.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/compactionHandoff.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextMemory.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextMemoryService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/contextTranscript.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/conversationTime.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/conversationUndoParticipants.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/loopEventFold.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/messageId.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/openToolExchange.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/toolResultRender.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextMemory/vacuousContent.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/contextProjector.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/contextProjectorService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/mediaProjection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/contextProjector/projection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/compaction-instruction.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/compactionOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/compactionUtils.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/fullCompaction.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/fullCompactionService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/strategy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/fullCompaction/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/goal/judge/goalJudgeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/goal/judge/judgeAgentProfile.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/goal/judge/judgePrompt.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/guardian/guardianService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/interruptionReminder/interruptionReminder.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/interruptionReminder/interruptionReminderOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/interruptionReminder/interruptionReminderService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/llmRequester.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/llmRequesterService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/llmRequestOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/llmRequester/toolCallIdNormalizer.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/engineOverride.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loop.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loopContinuation.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loopContinuationService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/loopService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/nativeBackgroundAgentTask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/stepRequest.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/stepRequestQueue.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/turnEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/loop/turnOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcp.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcpDiscoveryOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcpEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/mcpService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/output.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/tools/auth.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/mcp/tools/mcp.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/file-type.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/image-compress.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/image-format-policy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/image-originals.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/imageConfigBridge.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/kimiFileUrl.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaReadContext.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaRef.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaResolver.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/mediaResolverService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/promptMediaIntake.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/sessionMediaStore.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/sessionMediaStoreService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/videoUpload.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/webp-animated.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/webp-dec-wasm.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/media/webp-decode.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/flag.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/microCompaction.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/microCompactionOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/microCompaction/microCompactionService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/modeMutex/modeMutex.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/modeMutex/modeMutexService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionGate/permissionGate.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionGate/permissionGateService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/injection/permission-mode-auto-enter-reminder.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/injection/permission-mode-auto-exit-reminder.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/injection/permissionModeInjection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/permissionMode.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/permissionModeOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionMode/permissionModeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/permissionPolicy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/permissionPolicyService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/auto-mode-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/auto-mode-ask-user-question-deny.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/dangerous-command-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/default-tool-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/fallback-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/git-control-path-access-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/git-cwd-write-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/guardian-review.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/path-utils.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/sensitive-file-access-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/session-approval-history.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-allow.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-ask.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-deny.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/user-configured-rule.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/policies/yolo-mode-approve.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionPolicy/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/matchesRule.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/permissionRules.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/permissionRulesOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/permissionRules/permissionRulesService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/agentPlugin.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/agentPluginOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/agentPluginService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/plugin/index.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/pluginCommand/pluginCommand.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/pluginCommand/pluginCommandService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/context.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/profile.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/profileOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/profile/profileService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/prompt.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptMetadataText.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/prompt/promptStepRequests.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/replayBuilder/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/agentRuntime.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/runtimeBinding.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/runtimeBindingOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/runtimeBinding/runtimeBindingService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/scopeContext/scopeContext.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/shellCommand/shellCommand.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/shellCommand/shellCommandService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/state/agentState.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/state/agentStateService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/stepRetry/stepRetry.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/stepRetry/stepRetryService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/notificationXml.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/persist.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/printDefaults.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/taskOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/taskService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/tools/format.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/task/types.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tokenCounting/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tokenCounting/tokenCounting.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tokenCounting/tokenCountingOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolActivation/toolActivation.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolActivation/toolActivationService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolApproval/toolApproval.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolApproval/toolApprovalService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolDedupe/toolDedupe.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolDedupe/toolDedupeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolDisclosure/toolDisclosure.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/beforeToolExecuteEvent.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolExecutor.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolExecutorEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolExecutorService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolHooks.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolExecutor/toolScheduler.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/evaluate.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/toolPolicy.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolPolicy/toolPolicyService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/builtinToolAssemblyService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/toolContribution.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/toolRegistry.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolRegistry/toolRegistryService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolResultTruncation/toolResultTruncation.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolResultTruncation/toolResultTruncationService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent-background-disabled.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent-background-enabled.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent-fork.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agent.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/agentTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/agent/subagent-task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/ask-user-question.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/ask-user.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/askUserQuestionTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/ask-user-question/question-background-task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/edit/edit.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/edit/edit.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/edit/editTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/fetch-url/fetch-url.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/fetch-url/fetch-url.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/fetch-url/fetchUrlTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/configSection.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/envOverlay.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/github-request.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/github-tools.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/github/github.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/mainAgentOnly.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/bash.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/bash.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/bashTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/native-bash-process.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/bash/process-task.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/glob/glob.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/glob/glob.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/glob/globTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/grep/grep.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/grep/grep.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/grep/grepTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/read/read.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/read/read.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/read/readTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/write/write.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/write/write.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/os/write/writeTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/read-media-file/execute-media-read.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/read-media-file/read-media-file.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/select-tools/select-tools.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/select-tools/selectToolsTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-list/task-list.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-list/task-list.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-list/taskListTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-output/task-output.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-output/task-output.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-output/taskOutputTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-stop/task-stop.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-stop/task-stop.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-stop/taskStopTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/flag.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/task-wait.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/task-wait.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/task/task-wait/taskWaitTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/web-search/web-search.md` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/web-search/web-search.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/tools/web-search/webSearchTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/dynamicTools.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/flag.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelect.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectAnnouncements.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectAnnouncementsService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectSchemas.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectSchemasService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/toolSelect/toolSelectService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/undo/undo.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/undo/undoService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/cacheProbe.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/cacheProbeService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/errors.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/usage.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/usageEvents.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/usage/usageOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/userTool/userTool.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/userTool/userToolOps.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |
| `agent/userTool/userToolService.ts` | `turn_loop/*` (module: Rust counterpart as above) | PORTED (broad) | see top-level slice row |

### `app/` (215 files) — PARTIAL/NA-as-host

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `app/agentIdentity/agentIdentity.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentIdentity/agentIdentityService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentIdentity/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileCatalog.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileContribution.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileRegistry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/agentProfileRegistryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/builtinAgentProfileLoader.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/builtinAgentProfileLoaderService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/contribution.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/model-adaptations/claude-3.5-sonnet.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/model-adaptations/deepseek-v3.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/model-adaptations/gpt-4o.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/profile-shared.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/promptPrefix.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/agentProfileCatalog/system.md` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/auth.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/authService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/oauthProtocol.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/baidu/baidu.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/baidu/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/baidu/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/bing/bing.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/bing/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/bing/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/brave/brave.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/brave/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/csdn/csdn.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/csdn/fetchCsdnArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/csdn/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/duckduckgo/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/duckduckgo/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/duckduckgo/searchDuckDuckGo.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/engine-html.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/engine-http.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/engine-undici.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/exa/exa.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/exa/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/github/github.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/github/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/http3.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/juejin/fetchJuejinArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/juejin/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/juejin/juejin.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/linuxdo/fetchLinuxDoArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/linuxdo/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/linuxdo/linuxdo.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/sogou/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/sogou/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/sogou/sogou.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/startpage/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/startpage/parser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/startpage/startpage.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/web/fetchWebContent.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/web/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/zhihu/fetchZhihuArticle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/zhihu/index.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/engines/zhihu/zhihu.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/providers/local-web-search.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/providers/moonshot-web-search.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/webSearch.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/auth/webSearch/webSearchService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/authLegacy/authLegacy.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/authLegacy/authLegacyService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bashParser/bashParser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bashParser/bashParserService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bootstrap/bootstrap.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/bootstrap/bootstrapService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/capability.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/capabilityEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/capabilityService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/entries/context.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/entries/kimiCu.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/entries/kimiWebbridge.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/host.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/capability/types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/config.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configOverlayContributions.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configPure.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configSectionContributions.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/configService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/deprecations.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/migrations.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/sectionDiff.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/toml.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/config/tomlWriteback.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/editService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/fileEdit.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/fileEditService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/edit/textModel.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/event.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/event2.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/eventBus.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/eventBusService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/eventService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/event/fiberEventResolver.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/feature/featureManager.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/feature/featureManagerService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/feature/featureServiceContribution.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/file/fileService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/file/fileServiceImpl.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flag.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flagRegistry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flagRegistryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/flag/flagService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/gateway/gateway.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/gateway/gatewayService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/git.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/gitParsers.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/gitService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/git/workTree.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/hostFolderBrowser/hostFolderBrowser.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/hostFolderBrowser/hostFolderBrowserService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/builtInModelsDev.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/discovery.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/discoveryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/envOverlay.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/hostRequestHeadersAdapter.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/kosongConfig.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/kosongConfigService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDev.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDevImport.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDevImportService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/modelsDevUpstream.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/kosongConfig/oauthTokenAdapter.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/llmProtocol/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/llmProtocol/message.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/llmProtocol/usage.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/configLoader.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/configSection.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/configStore.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/oauthCoordinator.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/oauthService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpConfig/oauthStore.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpManagement/mcpManagement.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpManagement/mcpManagementService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpRegistry/mcpRegistry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/mcpRegistry/mcpRegistryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/archive.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/commands.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/github-resolver.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/manager.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/manifest.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/marketplace.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/plugin.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/pluginEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/pluginService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/source.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/store.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/plugin/types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/projectLocalConfig/projectLocalConfig.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/remoteControl/flag.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/scopes.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/file-source.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/manifest.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/sessionExport.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/sessionExportService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/wire-scan.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionExport/zip.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndex.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexMirrorService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexModel.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexProjector.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionIndex/sessionIndexSource.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionLegacy/sessionLegacy.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionLegacy/sessionLegacyService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionLegacy/sessionProtocol.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionManager/sessionLookup.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionManager/sessionManager.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/sessionManager/sessionManagerService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/state/appState.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/state/appStateService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/task/task.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/task/taskService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/agentTelemetryContext.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/agentTelemetryContextService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/cloudAppender.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/cloudTransport.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/consoleAppender.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/coreVersion.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/events.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/privacy.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/telemetry.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/telemetry/telemetryService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/providers/local-fetch-url.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/providers/moonshot-fetch-url.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/tools/fetch-url-types.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/web.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/web/webService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/errors.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/fileWorkspacePersistence.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspace.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspaceAlias.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspaceEvents.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspacePersistence.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspace/workspaceService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceAliases/workspaceAliases.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceAliases/workspaceAliasesService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceLifecycle/workspaceLifecycle.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceLifecycle/workspaceLifecycleService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceSessions/workspaceSessions.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `app/workspaceSessions/workspaceSessionsService.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |

### `debug/` (9 files) — NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `debug/debugCascade.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugCascadeService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugGraph.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugGraphService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugLedger.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/debugLedgerService.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/errors.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/index.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `debug/scopeTree.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |

### `features/` (253 files) — PORTED (majority)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `features/astron/flag.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/btw/btw.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/btw/btwFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/btw/btwService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/cronTask.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/errors.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/clock.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/cron-expr.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/format.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/internal/jitter.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-create/cron-create.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-create/cron-create.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-create/cronCreateTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-delete/cron-delete.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-delete/cron-delete.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-delete/cronDeleteTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-list/cron-list.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-list/cron-list.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/cron/tools/cron-list/cronListTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/dateChange.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/dateChangeFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/dateChangeService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/dateChange/disclosureBaseline.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/debugEvents/debugEvents.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/debugEvents/debugEventsFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/debugEvents/debugEventsService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/agent/agentExternalHooks.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/agent/agentExternalHooksService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/app/externalHooksRunner.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/app/externalHooksRunnerService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/externalHooksFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/matchHooks.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/runHook.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/internal/userPrompt.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/session/sessionExternalHooks.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/externalHooks/session/sessionExternalHooksService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/feature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/featureAssembly.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/featureAssemblyService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/featureRegistry.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistory.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryRetention.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/fileHistoryService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/fileHistory/flag.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/errors.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalDeadlineScheduler.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalDeadlineSchedulerService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/goalService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-active-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-blocked-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-budget-limited-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goal-paused-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/injection/goalInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/create-goal/create-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/create-goal/create-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/create-goal/createGoalTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/get-goal/get-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/get-goal/get-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/get-goal/getGoalTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/outcome-prompts.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/serialize.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/set-goal-budget/set-goal-budget.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/set-goal-budget/set-goal-budget.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/set-goal-budget/setGoalBudgetTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/update-goal/update-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/update-goal/update-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/tools/update-goal/updateGoalTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/goal/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interaction.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interactionFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interactionOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/interactionService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/interaction/sessionInteractions.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/exitPlanModeReview.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-exit-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-full-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-inline-full-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-inline-reentry-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-inline-sparse-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-reentry-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/plan-mode-sparse-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/injection/planModeInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/parsePlanToTodos.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/plan.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/planToTodoConverter.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/profile/plan.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/enter-plan-mode/enter-plan-mode.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/enter-plan-mode/enter-plan-mode.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/enter-plan-mode/enterPlanModeTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/exit-plan-mode/exit-plan-mode.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/exit-plan-mode/exit-plan-mode.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/plan/tools/exit-plan-mode/exitPlanModeTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/reminderFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/reminderService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/systemReminder.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/reminder/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/profile/init.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/profile/init.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/sessionInit.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/sessionInitFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/sessionInit/sessionInitService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/builtin.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/check-kimi-code-docs.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/check-kimi-code-docs.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/custom-theme.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/custom-theme.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/import-from-cc-codex.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/import-from-cc-codex.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/mcp-config.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/mcp-config.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/registry.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill/consolidate/SKILL.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill/review/SKILL.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill/SKILL.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/sub-skill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/update-config.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/update-config.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/write-goal.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtin/write-goal.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/builtinSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/errors.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/fileSkillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/inMemorySkillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/parser.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/registry.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/skillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/skillRoots.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/skillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/catalog/userFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/prompt.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/session/skillCatalog.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/session/skillCatalogData.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/session/skillCatalogService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skillFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skillOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/skillService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/tools/skill.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/tools/skill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/tools/skillTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/explicitFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/extraFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/pluginSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/rootFileSkillSource.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/runtimeSkillDiscovery.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/workspaceSkillCatalog.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/skill/workspace/workspaceSkillCatalogService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spill.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spillFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spillService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/spill/spillStore.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuard.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuardFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuardOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/staleGuard/staleGuardService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/enter-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/exit-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/injection/swarmInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/swarm.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/agent/swarmService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/configSection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/session/agentRunBatch.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/session/sessionSwarm.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/session/sessionSwarmService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/swarmFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/swarmOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agent-swarm-fork.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agent-swarm.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agent-swarm.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/swarm/tools/agent-swarm/agentSwarmTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoItem.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoListReminder.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/todoService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todo-list-write-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todo-list.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todo-list.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/todo/tools/todo-list/todoListTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tokenCounting/tokenCountingFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/flag.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/tower-mode-exit-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/tower-mode-full-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/tower-mode-sparse-reminder.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/injection/towerModeInjection.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/baseWip.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/frontmatter.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/git.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/index.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/paths.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/repoRoot.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/store.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/protocol/types.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/finding/finding.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/finding/finding.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/finding/findingTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/inbox/inbox.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/inbox/inbox.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/inbox/inboxTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/init/init.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/init/init.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/init/initTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/merge/merge.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/merge/merge.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/merge/mergeTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/mission/mission.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/mission/mission.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/mission/missionTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/plan/plan.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/plan/plan.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/plan/planTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/review/review.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/review/review.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/review/reviewTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/send/send.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/send/send.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/send/sendTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/spawn/spawn.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/spawn/spawn.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/spawn/spawnTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/status/status.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/status/status.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/status/statusTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/support.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/teardown/teardown.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/teardown/teardown.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tools/teardown/teardownTool.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tower-worker-overlay.md` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/tower.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerOps.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerRateLimit.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerRateLimitService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/towerService.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/tower/workerProfile.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |
| `features/usage/usageFeature.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (majority) | see top-level slice row |

### `kosong/` (62 files) — PORTED (split)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `kosong/contract/capability.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/generate.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/inspection.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/message.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/messageHelpers.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/provider.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/requestTrace.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/tokens.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/tool.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/contract/usage.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/catalog.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/catalogService.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/completionBudget.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/hostRequestHeaders.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/inspection.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/model.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/model.types.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelAuth.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelOAuth.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelRequester.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelRequesterImpl.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/modelService.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/model/thinking.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/protocol.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/protocolBase.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/protocol/protocolTrait.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropic-profile.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropic.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropic.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/anthropicHooks.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/anthropic/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/antigravity/antigravity.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/antigravity/antigravity.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/antigravity/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/google-genai/google-genai.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/google-genai/google-genai.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/google-genai/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/merge-user-messages.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/chat-completions-stream.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/index.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-common.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-legacy.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-legacy.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-responses.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openai-responses.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/openaiHooks.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/openai/reasoning-key.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/request-auth.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/bases/tool-call-id.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/protocolAdapterRegistry.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/provider.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providerDefinition.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi-errors.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi-files.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi-schema.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/kimi/kimi.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providers/standard.contrib.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/provider/providerService.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |
| `kosong/recordDiff.ts` | `llm/*` (module: Rust counterpart as above) | PORTED (split) | see top-level slice row |

### `mcpCore/` (16 files) — PORTED (gap: startup retry scheduler)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `mcpCore/client-http.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-remote.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-shared.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-sse.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/client-stdio.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/config-schema.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/configView.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/connection-manager.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/errors.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/callback-server.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/coordinator.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/provider.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/service.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/oauth/store.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/tool-naming.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |
| `mcpCore/types.ts` | `mcp/*` (module: Rust counterpart as above) | PORTED (gap: startup retry scheduler) | see top-level slice row |

### `os/` (17 files) — PARTIAL/NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `os/backends/host/hostClockService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostEnvironmentService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostFsService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostFsWatchService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostProcessService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/hostTerminalService.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/tools/rgLocator.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/backends/host/tools/runRg.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/configSection.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostClock.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostEnvironment.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostFileSystem.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostFsErrors.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostFsWatch.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/hostProcess.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/terminal.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |
| `os/interface/terminalErrors.ts` | `native/*` (module: Rust counterpart as above) | PARTIAL/NA | see top-level slice row |

### `persistence/` (13 files) — PORTED (Rust)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `persistence/backends/memory/inMemoryStorageService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/minidb/flag.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/minidb/miniDbQueryStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/appendLogStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/atomicDocumentStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/blobStoreService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/fileStorageService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/backends/node-fs/projectLocalConfigService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/appendLogStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/atomicDocumentStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/blobStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/queryStore.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |
| `persistence/interface/storage.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PORTED (Rust) | see top-level slice row |

### `program/` (2 files) — PARTIAL/NA-as-host

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `program/program.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |
| `program/programDependencies.ts` | `main.rs` (module: Rust counterpart as above) | PARTIAL/NA-as-host | see top-level slice row |

### `runtime/` (9 files) — NA

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `runtime/fakeRuntime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/localRuntime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimePath.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeProvider.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeRegistry.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeUnitHost.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/runtimeWorkspaceView.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |
| `runtime/standaloneRuntime.ts` | `-` (module: Rust counterpart as above) | NA | see top-level slice row |

### `session/` (76 files) — PORTED/PARTIAL

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `session/agentLifecycle/agentLifecycle.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/agentLifecycleService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/errors.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/mainAgent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/managedAgent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/explore-overlay.md` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/gitContext.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/profiles.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/profile/summary-continuation.md` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/agentLifecycle/subagentMetadata.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/approval/approval.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/approval/approvalService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/errors.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/mcp/ephemeralMcpServers.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/mcp/mergedConnectionView.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/mcp/sessionMcpHandle.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/process/processRunner.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/process/processRunnerService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/question/question.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/question/questionService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionActivity.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionActivityService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionOutcomeMirror.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionActivity/sessionOutcomeMirrorService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionAgentProfileCatalog/agentProfileCatalogSeed.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionAgentProfileCatalog/sessionAgentProfileCatalog.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionAgentProfileCatalog/sessionAgentProfileCatalogService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionContext/sessionContext.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionInstructions/instructionsProvider.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionLog/sessionLogService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/promptMetadata.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/sessionMetadata.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/sessionMetadataService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionMetadata/sessionMetaEvents.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/agentTitlePromptSource.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/agentTitlePromptSourceService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/flag.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/sessionTitle.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionTitle/sessionTitleService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicy/sessionToolPolicy.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicy/sessionToolPolicyService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicyGate/sessionToolPolicyGate.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/sessionToolPolicyGate/sessionToolPolicyGateService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/state/sessionState.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/state/sessionStateService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/acpBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/backendCompat.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/claudeCodeBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/codexBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/codexWire.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/configSection.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/flag.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/subagentBackend.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/backend/subagentBackendService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/configSection.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/flag.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/forkCompat.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/mirrorAgentRun.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/persistentSubagent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/persistentSubagentService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/runAgentTurn.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/spawn.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagent.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagentModelsValidation.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagentModelsValidationService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/subagent/subagentService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/terminal/terminalService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/tokenCounting/sessionTokenCounting.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/tokenCounting/sessionTokenCountingService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/tokenCounting/tokenCountingAgentModel.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/usage/sessionUsage.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/usage/sessionUsageService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/usage/usageAgentModel.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/workspaceContext/workspaceContext.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/workspaceContext/workspaceContextService.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |
| `session/workspaceInfo/workspaceInfo.ts` | `session/mod.rs` (module: Rust counterpart as above) | PORTED/PARTIAL | see top-level slice row |

### `state/` (7 files) — PARTIAL/NA (DI)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `state/agentEffect.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/agentModel.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/errors.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/eventDispatcher.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/eventDispatcherService.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/state.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |
| `state/stateContribution.ts` | `storage/state_store.rs` (module: Rust counterpart as above) | PARTIAL/NA (DI) | see top-level slice row |

### `tool/` (10 files) — PORTED (guarded subset: `tools/args_validate.rs`)

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `tool/args-validator.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/input-schema.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/native-glob-match.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/output-accumulator.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/path-access.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/result-builder.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/rule-match.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/tool-args-parse.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/toolContract.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |
| `tool/toolInputDisplay.ts` | `tools/*` (module: Rust counterpart as above) | PORTED (guarded subset: `tools/args_validate.rs`) | see top-level slice row |

### `wire/` (12 files) — PARTIAL/NA journal model

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `wire/errors.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/migration.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.1.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.2.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.3.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.4.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.5.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/migration/v1.6.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/record.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/repair.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/wire.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |
| `wire/wireService.ts` | `rpc/types.rs` (module: Rust counterpart as above) | PARTIAL/NA journal model | see top-level slice row |

### `workspace/` (54 files) — PORTED

| v2 file | Rust counterpart (module boundary) | status | tests |
|---|---|---|---|
| `workspace/sandbox/sandbox.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/coldSessionArchive.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/internal/addressing.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/internal/forkTurnSlice.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/sessionLifecycle.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/sessionLifecycleEvents.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/sessionLifecycle/sessionLifecycleService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/state/workspaceState.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/state/workspaceStateService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/configSection.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/explicitAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/explicitAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/extraAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/extraAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentFile.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentFileDiscovery.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentProfileFromFile.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/agentRoots.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/paths.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/systemFile.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/internal/types.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/pluginAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/pluginAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/userAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/userAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/workspaceAgentProfileLoader.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceAgentProfileLoader/workspaceAgentProfileLoaderService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceContext/workspaceContext.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceDirs/workspaceDirs.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceDirs/workspaceDirsService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fs.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fsService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fsWatch.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/fsWatchService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/errors.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/fsProcess.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/fsSearch.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/rgLocator.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceFs/internal/runRg.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceGit/workspaceGit.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceGit/workspaceGitService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstance/workspaceInstance.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstance/workspaceInstanceManager.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstance/workspaceInstanceManagerService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstructions/workspaceInstructions.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceInstructions/workspaceInstructionsService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcp/workspaceMcp.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcp/workspaceMcpService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcpConfig/workspaceMcpConfig.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceMcpConfig/workspaceMcpConfigService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceTrust/trustRecord.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceTrust/workspaceTrust.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |
| `workspace/workspaceTrust/workspaceTrustService.ts` | `workspace_trust.rs` (module: Rust counterpart as above) | PORTED | see top-level slice row |

- `480019652d` (2026-11-06) — chore: sync release/2026-11-06 back to main (#4733)
  - ported: `tools/registry.rs` implements v2's active/pending registration state machine with canonical version signatures; `McpManager` registers every discovered tool there, keeps `list_tool_infos` on the pinned active schema, exposes `list_live_tool_infos`/`has_pending_tool`, and unregisters on every removal path. `tool_select_state::observe_live_updates` emits a once-per-signature `tool_update_pending` reminder delivered by a run_turn injection provider. Pinned by registry, MCP manager, tool-select-state and two end-to-end run_turn tests; the rest of the commit is release/web-bundle churn.
- `1dd5578649` (2026-11-06) — feat(kap-server): stream agent conversations as turns and items (#4663)
  - not-applicable against the user's fork: that checkout has neither the commit nor `kap-server/src/protocol/conversation.ts`. The engine-side subset already landed here (block-indexed deltas, `turn.step.stream_reset`, classified projection, dropped-partial sealing and transcript handling) is pinned by `ui_slice_integration`; the host conversation feed and durable replay hooks do not exist on either side of this comparison.
- `df90d90ec0` (2026-11-05) — fix(agent-core-v2): refresh tools without dynamic loading (#4720)
  - ported: initial-snapshot reconciliation in `shape_tool_table`/callbacks, revision-ledger announcements in `ToolSelectState`, and `render_runtime_tool_change_reminder` with a real history-shaping channel (`shape_unsupported_dynamic_tool_history` called from `run_turn` before hooks/provider). An incapable model sees v2's final reminder wording instead of a stale `select_tools` announcement; pinned by unit, disclosure and end-to-end run_turn tests.
- `bae32119ca` (2026-11-05) — fix(agent-core-v2): reconcile tool definitions after updates (#4719)
  - ported: `ToolSelectState` holds initial/current/changes with a per-revision delta walk that filters added/removed by deferred disclosure; canonical schema/version signatures decide pending-vs-active registration, and `NativeToolset.take_disclosure_announcement` reconciles the live registry each turn and walks changes since the last announced revision. Pinned by tool_select_state, registry, MCP manager and turn-loop tests.
- `131c21a35a` (2026-11-05) — chore: sync release/2026-11-05 back to main (#4709)
  - not-applicable: the only engine-side source change is Kimi's empty-`content` deletion for a system message carrying `tools`. Rust projects tools only at the request root, so that message shape cannot occur.
- `2be062e8ea` (2026-11-05) — fix: route session status and git data through bound environments (#4698)
  - not-applicable against the single-process engine: it has no SSH/Docker environment providers or sessionWorkDir routing. Its local binding is now exposed on the user-fork-shaped `/api/v1/debug/session/:id/agent/:id/environment-binding` route with `cwd` and `environmentId=local`, pinned by `server::debug::tests::test_debug_inspect_snapshots_and_unknown_404`. Environment-scoped git cache invalidation has no producer here.
- `2a6d1cf570` (2026-11-05) — feat: add session title generation telemetry (#4699)
  - ported: `session/title.rs` reports each attempt over `host/telemetry` with v2's trigger/source/outcome/duration_ms and success-only title_chars/title_outcome; `force` reaches the engine from napi and stdio RPC. Pinned by 14 title tests and a trigger mutation check. Remaining divergence: `title_outcome` is always `ok` because both title producers return only the truncated string, so a raw title over the cap cannot be detected; v2 reports `truncated`.
- `8fc5e22f96` (2026-11-04) — fix: clarify wire version upgrade guidance (#4662)
  - ported: the public upgrade sentence is used by both wire refusals; the Rust error keeps required/supported versions for diagnostics. Host/CLI/TUI `WIRE_VERSION_TOO_LOW` details propagation needs a typed error-code channel the engine does not have.
- `f72dc7e1ca` (2026-11-04) — feat(agent-core-v2): enforce minimum wire reader version (#4638)
  - ported: `min_protocol_version` row stamp + shared reader gate, both SQLite schemas with idempotent ALTERs, NULL reads as current. Pinned by event-store/session-store tests and a mutation check.
- `444c609e8e` (2026-11-03) — feat(agent-core-v2): allow private addresses in local FetchURL (#4650)
  - ported: the local FetchURL tool path and the napi binding both default `allow_private` to true, matching v2's local-fetch semantics; plugin archive download keeps its own `allow_private=false` validator. The stale napi SSRF test asserted the old refusal — it now pins the new behavior (private address attempted, network error instead of an up-front refusal).
- `a5724dd32e` (2026-11-03) — fix(tui): track steer read state across turns (#4627)
  - ported: `turn.prompt` / `turn.started` echo the engine-minted `promptId`, and `TurnEvent::Steer` is emitted by `SteerQueueCallbacks::drain_steers` — the drain where a steer is consumed into context, matching v2's `TurnSteer` timing. `sessionEnqueueTurn` returns `TurnSubmission`; `Session.steer()` returns `PromptSubmissionResult`. **Rust UI is the complete module**: `EntryOp::SetPending` + `EntryStore::is_pending` + `render::style_for_entry` dim the row; `kimi-ui-core::steer::SteerDelivery` owns the batch/receipt/ack/release state machine (including the acknowledgement that races the receipt); `Projector::note_user_entry` allocates a host-injected row through the per-turn allocator (it lands between the pre- and post-steer text) and detaches the open stream; `kimi-tui-app` adds `UiCommand::Steer`, the turn-running flag and the steer API; `kimi-tui-host` allocates the row on submission and clears it from `turn.steer` / `turn.started.promptId`. Pinned by the Rust drain test, the UI-slice ordering test, 7 core state-machine tests, an end-to-end host test, and 3 TS TUI flow tests; the two host/TUI ack paths and the drain emission were mutation-checked. Two apparent gaps are non-cases, each now pinned and documented at the code site: `turn.steer` always carries `promptIds` (drain-only emission), so no text-matching fallback is needed; and a steer its turn did not reach is drained by the next turn (`a_steer_that_missed_its_turn_is_read_by_the_next_one`), so pending state must survive `turn.ended` rather than being reset. A steer admission is never `queued` (`an_active_or_new_turn_admission_is_never_queued`), so the host needs no optimistic row for that state. The one deliberate adaptation is that the row is allocated after the receipt, not at the keystroke: `note_user_entry` flushes and detaches the open text stream, so allocating for a steer the engine refuses would split an answer for nothing. **Real-path verification (2026-11-07).** `kimi-tui-host/tests/real_transport_steer.rs` runs the chain in one process against a local OpenAI-shaped SSE stub: real `NativeHttpLlm` + sink → session pump → turn loop → steer queue → `drain_steers` → `TurnEvent::Steer` → bus → `Bridge` → `UiState` → `render::entry_lines`. It asserts the stub's request bodies (the steer sits after the assistant tool call and its result), the bus order (`turn.prompt`/`turn.started` → `llm.delta`* → `turn.steer` with `promptIds` equal to the receipt's id → answer deltas → `turn.ended`), the exact `SteerJournal` stage sequence for the host-minted prompt id (`CommandAccepted → QueuePushed → Admitted{steered} → Receipt → RowAllocated → Drained → Acknowledged`), that the pended and cleared `EntryId` are the same row (`turn-0`), no duplicate rows, and no contract violations. A second test in the same file starts from `build_engine` — the config-resolved provider path `main` uses — and asserts the same stage sequence plus the applied-op trace (one `Upsert` for the row, then `SetPending(true)` and `SetPending(false)` on that same id). Two observation seams were added for this: `SteerJournal` (a shared ordered stage log; both sides record into it) and `kimi-tui-app`'s `ops-trace` feature (applied-op trace + last steer receipt id); the host mints the prompt id via `enqueue_turn_with_id` so every hop shares one key. The run found a real mid-path defect: `TurnEnded` sealed a stream whose last segment had already been flushed with `is_final = false` at `llm.step.end`, recording `SealWithoutFinal` on every real turn. The projector now closes such a stream with an empty final segment; pinned by `a_step_flush_before_turn_end_does_not_record_a_tail_loss`. Not exercised: `App::run()` against a TTY (this environment cannot drive one); the paint hop is `render::entry_lines` over the real store, not a mounted widget. **The skill path is ported too and verified the same way.** A Ctrl-S steered skill activation is dimmed: `activateSkill` returns `SkillActivationResult { activationId, submission }` (the busy branch keeps the receipt it used to drop), the card carries `steerPending`, and the same `turn.steer` acknowledgement clears it. `apps/kimi-code/test/tui/kimi-tui-real-steer.test.ts` drives a real `KimiTUI` over the native harness — temp `config.toml` provider pointed at a local OpenAI-shaped SSE stub, a real workspace skill — and asserts the second request body carries the rendered skill body after step one's `tool_call`, and that the card line is dimmed (`ESC[2m`) while pending and plain after the acknowledgement, on the same transcript entry. The TUI answers "where is this prompt now" the way the engine does: `apps/kimi-code/src/tui/steer-trace.ts` keeps an append-only hop log (`accepted` -> `pending` -> `submitted` -> `acknowledged` | `released`) keyed by prompt id, and the test asserts the exact sequence, the current stage, and that no other prompt id appears in it.
- `80193374eb` (2026-11-03) — feat: add plugin marketplace and native mcp oauth (#4525)
  - ported: the marketplace entry now parses and exposes `sha256sum`; catalogued remote installs load a verified archive from `<home>/plugins/cache/archives`, refuse digest mismatches, replace corrupt entries and prune the cache at v2's 500 MiB bound. MCP OAuth cancel/invalidate and concurrent-grant protection are already implemented. Pinned by `server::plugin_archive::tests::{a_verified_archive_is_cached_and_reused,a_verified_archive_refuses_a_digest_mismatch}`, `server::plugins::tests::test_load_marketplace_from_custom_repo_root`, and the MCP OAuth/manager tests.
- `077239666a` (2026-11-03) — feat(goal): edit the current goal objective in place (#4568)
  - ported: `GoalState::edit_objective` + `objective` state action + `PATCH /api/v1/sessions/{id}/goal` (400/404) + REST manifest/schemas, with unit/state-store/REST tests and a mutation check. Remaining host-only: objectiveRevision turn gating, replay fold, SDK method.
- `011b1d4163` (2026-11-03) — feat(kap-server): push agent, usage, goal and todo state over WebSocket (#4606)
  - not-applicable: every changed source file is in the retired kap-server; no agent-core-v2 source changed, and reintroducing these frames would add a second protocol stack.
- `b4da921509` (2026-11-02) — feat(agent-core-v2): replace hidden media tool results with omission notices (#4601)
  - not-applicable: no `ReadMediaFile` tool in this fork; `Read` refuses media reads for a model without `image_in`/`video_in` at execution time with a path-bearing reason, and `MediaResolver` already leaves `<image/video path=...>` for unaccepted references. There is no hidden media-bearing tool result to rewrite.
- `aca85b92fc` (2026-11-02) — feat(agent-core-v2): link cache breaks to baseline requests (#4489)
  - not-applicable against the user's fork: its checked-out `agent/usage/cacheProbeService.ts` still implements the older client-side cache-break behavior, and the commit is not an ancestor of its `main`. The Rust engine keeps the matching client-side behavior; requester-side hashes and telemetry would introduce a contract the reference fork has not adopted.
- `ebc2a9f960` (2026-11-01) — feat: sync running tasks over WebSocket and drop the REST task API (#4535)
  - not-applicable against the user's fork: the commit is not an ancestor of its `main`, and the checked-out `packages/transcript/src/ops/operation.ts` still declares `TranscriptTask` / `task.upsert`. The Rust engine retains the same projection and v1 transcript contract; `task.watch/list` is part of the newer upstream protocol that the reference fork has not adopted.
