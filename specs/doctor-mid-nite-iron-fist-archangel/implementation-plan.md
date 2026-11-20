# v2 Agent Tools Behavioral Alignment Implementation Plan

> **For agentic workers:** Use `superpowers:executing-plans` to implement this plan in the current session after its review. Steps use checkboxes. No subagent execution or commits are authorized by this plan.

**Goal:** Align the entire v2 `agent/tools` behavior surface with the Rust engine's actual model-visible and executable tool paths.

**Architecture:** Trace the production path before choosing the Rust implementation to change. Land one semantic family at a time, including its schema, direct consumers and behavioral tests. Keep verified equivalence distinct from implemented-but-unverified and from missing production wiring.

**Tech Stack:** Rust, existing Tokio/reqwest/serde_json implementations, Cargo tests; Bun for existing repository gates and executing reference fixtures where available. No new dependencies are planned.

**Spec:** `specs/doctor-mid-nite-iron-fist-archangel/{requirements,design,tasks}.md`.

## Global constraints

Behavior is determined by the user-maintained reference `D:/kimi/kimi-code/packages/agent-core-v2/src/agent/tools`. Consult `.tmp/v2-ref-upstream` separately when investigating differences. Apply the repository instructions and `reports/runbooks/dev-workflow.md`; preserve concurrent changes and do not rewrite shared baselines. Compilation-only fixes do not close parity tasks.

## Review focus

1. A v2-shaped Rust wrapper without a production constructor: the tested behavior must also be exercised through registration and dispatch (Tasks 1 and 8).
2. Response overflow in the middle of UTF-8 and across multiple chunks: retain the v2 byte cap, lossy decoding and observed-overflow marker, and stop reading after overflow (Task 2).
3. A cancellation during a pending request, wait or foreground/background transition: preserve cancellation reason and distinguish cancelling a wait from stopping its task (Tasks 2, 5 and 6).
4. A file or attachment changes between resolution and execution: revalidate through the correct environment/session store and preserve reference-based pagination (Task 4).
5. A model sees a schema/default or description that disagrees with execution: compare the advertised tool table with the parser and implementation it reaches (Tasks 3–8).

## Files and production seams

- Behavior owners: `packages/kimi-agent/src/agent/tools/**` and the existing implementation paths below.
- Production entry points: `src/tools/builtin_assembly.rs`, `src/tools/dispatch.rs`, `src/tools/tool_policy.rs`, relevant `src/agent/tool_registry/**` / `tool_executor/**`.
- Existing legacy callers/owners: `src/tools/{file_ops,search,bash_exec,powershell,fetch_url,web_search,agent_tool,task_tools,select_tools}.rs`, `src/native/**`, `src/storage/task_runner.rs` as actually referenced. Verify existence and module declarations; historical backlog paths can be obsolete.
- Existing test owners: tool-family inline tests; `src/tools/tests.rs`; `src/tools/ask_user_question/tests.rs` if present; `src/storage/task_runner/tests.rs`; existing integration tests under `tests/` that drive the changed seam.
- Evidence: incrementally update `reports/runbooks/v2-parity-ledger.md`; use this spec's `progress.md` for work-in-progress inventory and verification limits.

## Task 1: Reviewed file inventory and caller map

**Files:** Read all reference units below and Rust counterparts in `src/agent/tools`; update `progress.md` with individual file dispositions. Read `builtin_assembly.rs` and `dispatch.rs` plus their callees.

**Interfaces:** Produces a reviewed mapping from each reference unit to its behavior owner, production call chain, tests and unresolved differences. This mapping controls subsequent batches; candidate paths are not equivalence verdicts.

- [ ] Enumerate and read the 50 currently listed `.ts` units and the 18 description assets. Record the reference commit and dirty-state/digests for the files being compared so later edits cannot silently move the target.
- [ ] For every unit below, record one row with `reference path`, `Rust owner`, `registration -> dispatch -> implementation`, `evidence/test`, `disposition`.
- [ ] Preserve the current in-flight changes and reconcile the old 11-row backlog without marking file movement as behavioral completion.

Reference units, relative to `agent/tools`:

| Family | Units requiring individual rows |
|---|---|
| Common | `fileReadSource.ts`, `fileMemory.ts`, `mainAgentOnly.ts` |
| Agent | `agent/agent.ts`, `agent/agentTool.ts`, `agent/subagent-task.ts` |
| Questions | `ask-user-question/ask-user-question.ts`, `ask-user-question/askUserQuestionTool.ts`, `ask-user-question/question-background-task.ts` |
| Edit | `edit/edit.ts`, `edit/editTool.ts` |
| Fetch | `fetch-url/fetch-url.ts`, `fetch-url/fetchUrlTool.ts` |
| GitHub | `github/configSection.ts`, `github/envOverlay.ts`, `github/github.ts`, `github/github-tools.ts`, `github/github-request.ts`, `github/github-proxy.ts`, `github/github-shape.ts` |
| Bash | `os/bash/bash.ts`, `os/bash/bashTool.ts`, `os/bash/process-task.ts` |
| Glob/Grep | `os/glob/glob.ts`, `os/glob/globTool.ts`, `os/grep/grep.ts`, `os/grep/grepTool.ts` |
| PowerShell | `os/powershell/powershell.ts`, `os/powershell/powershellTool.ts` |
| Read/Write | `os/read/configSection.ts`, `os/read/read.ts`, `os/read/readTool.ts`, `os/write/write.ts`, `os/write/writeTool.ts` |
| Shell results | `os/shell/shellToolResults.ts` |
| Media | `read-media-file/read-media-file.ts`, `read-media-file/execute-media-read.ts` |
| Selection | `select-tools/select-tools.ts`, `select-tools/selectToolsTool.ts` |
| Tasks | `task/task-list/task-list.ts`, `task/task-list/taskListTool.ts`, `task/task-output/task-output.ts`, `task/task-output/taskOutputTool.ts`, `task/task-stop/task-stop.ts`, `task/task-stop/taskStopTool.ts`, `task/task-wait/task-wait.ts`, `task/task-wait/taskWaitTool.ts`, `task/task-wait/flag.ts` |
| Search | `web-search/web-search.ts`, `web-search/webSearchTool.ts` |

**Check:** Re-enumerate the reference directory and compare sorted paths to the rows. No uncovered unit or removed-unit zombie is acceptable. Description assets must map to advertised strings even when embedded as Rust constants.

## Task 2: Correct and verify GitHub request limits and cancellation

**Files:** `src/agent/tools/github/{github,github_request,github_tools,github_legacy}.rs`; existing inline tests.

**Interfaces:** `GitHubToolSpec.max_bytes: Option<usize>` -> `GitHubRequestOptions.max_bytes`; `GitHubRequestResult.next_page: bool`, `truncated_bytes: usize`. Existing `github_request(...)` remains the single HTTP request owner.

- [ ] Add meaningful failing tests before modifying the current unverified patch: PR diff limit is **200_000**, not `200 * 1024`; body below/exactly/above the limit; split UTF-8; cancellation before response and during streaming.
- [ ] Correct the constant/spec/tests to `200_000`. Retain the global maximum `5 * 1024 * 1024` and cap per-request settings against it.
- [ ] Match `github-request.ts::readResponseBody`: stop/cancel on the first overflowing chunk, retain only its allowed prefix, count its observed overflow and emit `\n[...truncated N+ bytes]`. Do not drain an unbounded remainder to calculate an exact total.
- [ ] Match `parseNextPage` to the reference's literal `rel="next"` rule; avoid an unrequested broader parser. Test absent/next/previous/multiple links.
- [ ] Make the loopback test independent of transport chunk size: serve separately acknowledged chunks, record whether another chunk is requested/connection closes, assert only observed overflow rather than total body size. Read full request headers and compare header names case-insensitively. Use a no-proxy injected client for the loopback test.
- [ ] Run `cargo test --lib agent::tools::github::github_request::tests:: --no-default-features --features cli --offline` from `packages/kimi-agent`. Expected: the selected cases execute and pass. A lib-test compile failure means this task remains unverified, even with no GitHub diagnostics.

## Task 3: GitHub specifications, projections and real dispatch

**Files:** `src/agent/tools/github/{github,github_tools,github_request,github_legacy,config_section,env_overlay,mod}.rs`; create `github_shape.rs` / `github_proxy.rs` only as translations of the corresponding reference responsibilities if no existing owner exists. Necessary config and dispatch changes belong to this family.

**Interfaces:** Preserve v2's optional per-spec shape function over `(body, nextPage, parsed arguments)`; an unavailable projection falls back to the original body. The legacy adapter must preserve result fields and call the same implementation, not duplicate projections.

- [ ] Add fixtures that pin each reference `GITHUB_SPEC_EXTRAS` mapping, including list headers, omitted fields, ordering, unknown/malformed JSON fallback, workflow wrapped lists, search totals, tree limits, base64 text/binary content and `maxChars` (1–40_000).
- [ ] Port `github-shape.ts` and connect every reference mapping. Remove this session's invented `(has_more: true)` suffix; pagination belongs to the reference projection header, and unshaped tools must not gain a new suffix.
- [ ] Compare the complete GitHub spec set, input defaults and URL/query/body mappings, including tools currently absent from Rust. Match reference failure detail limits (400 below 500, 2000 at 500+) and the rate warning below 100.
- [ ] Port reference retry/cache/proxy behaviors as separate testable GitHub sub-batches, using the existing config/fetch boundaries: retry statuses and retry-after limits, ETag identity/retention/304 reuse, proxy domain and mirror eligibility/fallback. Do not call the public GitHub service to test these.
- [ ] Exercise through `github_legacy::execute_github_tool` and the `NativeToolset` GitHub branch; assert request capture and exact model-visible result, with representative read and mutation metadata.
- [ ] Run `cargo test --lib agent::tools::github:: --no-default-features --features cli --offline` and the affected existing dispatch tests selected by exact test names. Record unexecuted tests as unverified.

## Task 4: Filesystem and media behavior

**Files:** `src/agent/tools/{edit,os/read,os/write,os/grep,os/glob,read_media_file}/**`, shared file source/memory modules; actual callees in `src/tools/{file_ops,search}.rs` / native modules and dispatch as found in Task 1.

- [ ] Add differential cases for accepted/default/invalid inputs, pagination continuation, hidden/ignored/sensitive files, symlink targets, UTF encodings, text/media branching, model capability refusals and file-memory augmentation.
- [ ] For attachments, test the production Read route using `kimi-file://id`, session store open/read, changed/deleted content, ranged/head/line reads, and error location. Keep the restored `file_read_source.rs` unchanged unless consolidating it is necessary to align that tested production route.
- [ ] Port each confirmed discrepancy and remove duplicate runtime behavior only where the replacement preserves its full observed feature inventory.
- [ ] Run exact changed-tool test filters and existing file/media dispatch tests; demonstrate that advertised schemas and actual entry points use the tested owner.

## Task 5: Shell and process task lifecycle

**Files:** `src/agent/tools/os/{bash,powershell,shell}/**`, actual `src/tools/{bash_exec,powershell,shell_results}.rs`, direct task snapshot boundary in `src/agent/task/task.rs`.

- [ ] Pin v2 command encoding/quoting, default and maximum timeout, no-background gate, detach/timeout-detach, output merging and exit/failure messages.
- [ ] Use fake host processes to assert stdout/stderr start observing at spawn, output survives the **250 ms** drain grace, cancellation disables foreground callbacks without discarding durable output, and disposal/release occurs once.
- [ ] Exercise long output through real Shell -> task output snapshot -> spill reference. Assert camelCase fields, output path, byte size and TaskOutput/Read hint; test missing full-log availability.
- [ ] Run exact shell/process/runner filters after each semantic change. Do not mark snapshot serialization alone as a complete Shell tool port.

## Task 6: Questions, task tools and Agent

**Files:** `src/agent/tools/{ask_user_question,task,agent}/**`, `src/tools/{task_tools,dispatch,agent_tool}.rs`, direct task runner/subagent callers required by the tested behavior.

- [ ] Pin TaskList terminal/detached visibility and limits; TaskOutput field names/status/tail/full-log hints; TaskStop idempotent terminal state, cancellation reason and partial-output/resume hint; TaskWait named/any/no-task/timeout/steer/extra-completion cases, telemetry, progress and delivered-notification suppression.
- [ ] Pin AskUserQuestion unique question/option rejection, noninteractive and unsupported client, dismissed answer, snake-case multi-select versus deprecated camel-case behavior, background availability and cancellation cleanup at the callback boundary.
- [ ] Pin Agent spawn/resume ownership/profile/backend/fork/model gates, foreground cancellation, detach semantics, terminal and partial-output outcomes. Use actual registration/service/task calls so lifetime fixes are not the only observed result.
- [ ] Port each discrepancy with its callers, run exact task/question/Agent test filters and the corresponding NativeToolset scenario, and record unresolved shared-service dependencies without inventing fallback behavior.

## Task 7: Web, selection and remaining units

**Files:** `src/agent/tools/{fetch_url,web_search,select_tools}/**`, their actual `src/tools` callees, main-agent-only and file-memory helpers.

- [ ] Pin FetchURL passthrough/extracted/empty/error/cancellation/citation text using a local fetch fake; WebSearch absent provider/results/empty/error classification and rendered references.
- [ ] Pin selection exact names/query/schema rules, disabled model, policy-denied/stale/unknown tool, duplicate selection and escalation; assert load availability on the next actual request.
- [ ] Compare the 18 prompt/description assets and common helpers against the advertised schemas and model-visible strings.
- [ ] Resolve every inventory row to verified behavior, implemented-but-unverified, missing, or a documented equivalent mechanism. No unnamed missing or unconnected unit can disappear from the inventory.

## Task 8: Family evidence and final acceptance

**Files:** spec `progress.md`, incremental entries in `reports/runbooks/v2-parity-ledger.md`; relevant existing parity manifests only after source review establishes a justified verdict.

- [ ] After each family, record exact reference identity/source locations, Rust production chain, tests/exit codes and residual divergences. Do not upgrade verdicts based only on compiler output.
- [ ] Run the diff-relevant read-only gates (`check:parity`, `check:engine-i18n`, `check:rust-module-deps`, `check:architecture`, relevant manifest gate); distinguish recorded baseline drift from new failures. Do not refresh fingerprints amid concurrent writes.
- [ ] Review full session diff versus the inherited dirty state; check stale comments/description copy, user work preservation and generated/untracked test outputs.
- [ ] Once the harness becomes buildable, execute all focused family tests and the actual engine scenario for each family. Run the package acceptance commands only after targeted failures are resolved.
- [ ] Report module completion only if all inventory behavior units are verified and wired. Otherwise report the exact blocked/unimplemented rows; user decides whether a partial delivery is acceptable. No commit or push step is included.

## Plan self-review

The whole tool module remains in scope; these tasks divide execution, not the requested deliverable. The source-confirmed PR diff byte value overrides the earlier erroneous 200 KiB wording. Task 2 tests stream closure, Tasks 3/8 test production wiring, Tasks 4/5 test stale storage/process state, Task 6 tests cancellation and Task 7 tests schema-to-execution consistency. Existing test-harness failures are a verification blocker, not authorization to remove tests or assert equivalence.
