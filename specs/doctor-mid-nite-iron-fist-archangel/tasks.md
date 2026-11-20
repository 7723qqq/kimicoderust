# Tasks: v2 `agent/tools` Behavioral Alignment

## 1. Build the reviewed inventory and batch map

Read every v2 source unit under `agent/tools`; map it to Rust source, existing legacy implementations, registration and real dispatch. Reconcile the map against `reports/v2-parity/porting-backlog.md` and current working-tree ownership without rewriting either file wholesale.

Acceptance criteria:
- Every v2 `agent/tools` source unit has a listed Rust candidate or an explicit missing/unresolved disposition.
- Each candidate identifies its actual production registration/dispatch path, or is explicitly marked unconnected.
- Changed paths inherited from other work are identified and preserved; no unrelated path is reverted.
- The proposed batches name their behavior family and direct shared-contract dependencies.

## 2. Complete the in-progress GitHub request behavior slice

Compare `github-tools.ts`, `github-request.ts`, `github-shape.ts`, and their referenced tests with `github_tools.rs`, `github_request.rs`, `github_legacy.rs`, and the actual dispatch adapter. Complete the PR diff response bound and list-pagination behavior at the caller-visible result boundary. Determine whether the remaining v2 response projections are in this batch or a separately tracked GitHub sub-batch; do not imply raw response bodies are equivalent to projected v2 output.

Acceptance criteria:
- A local test proves `GitHubGetPRDiff` sends `Accept: application/vnd.github.diff`, retains no more than exactly 200_000 bytes of response body, and reports the observed truncated-byte count and marker in the production result path.
- A local paginated response with `Link: ...; rel="next"` produces the same observable `has_more` information as v2; a response without a next link does not.
- Non-diff requests remain capped at 5 MiB, and a spec cannot raise that global cap.
- GitHub GET specs report `sideEffect=read` / `repoMutation=none`; specs marked mutating report `write` / `possible`.
- Tool-specific request/response shape divergences discovered in the comparison are either fixed with tests or explicitly left unresolved in the ledger; none are silently described as aligned.

## 3. Align filesystem, media, and shell tools

Compare Read/Grep/Glob/Write/Edit, media-read, Bash, PowerShell, and process-task behavior with their actual execution path. Focus on concrete v2 contracts such as file-source lifetime, workspace/symlink boundaries, side effects, process cancellation, stream draining, and persisted output paging. Keep `file_read_source.rs` at its restored HEAD state unless a reviewed production-path change requires touching it.

Acceptance criteria:
- Each changed behavior has a test that reaches the actual tool dispatch or service seam and asserts its v2-observable result.
- Attachment reads use the current session store/file id and preserve stat, bounded reads, line decoding, and missing-after-open failure behavior where those paths are in scope.
- Shell/process completion retains output already read before exit or cancellation; timeout/detach and TaskOutput behavior match the cited v2 boundary.
- Workspace and sensitive-file refusals are asserted on the production tool route, not only helper predicates.
- Any behavior blocked by an unrelated shared contract is recorded with the exact caller, missing contract, and test/build command that cannot execute.

## 4. Align task, question, and agent tools

Compare TaskList/TaskOutput/TaskStop/TaskWait, AskUserQuestion, Agent, and their helper files against the v2 source and real dispatch. Exercise foreground/background, cancellation, repeat/terminal state, notification delivery, argument aliases, and partial output only where v2 defines those behaviors.

Acceptance criteria:
- Each changed result field/status/message and each branch-critical alias is asserted against a v2-referenced fixture or explicit assertion.
- Cancellation and stop tests show whether the task is stopped, detached, still running, or merely interrupted; no inferred equivalence from matching enum names is accepted.
- User-question foreground/background and multi-select payloads are checked at the callback/task boundary.
- Agent/subagent lifecycle tests cover cancellation/partial-output behavior through the actual service or task registration path.

## 5. Complete remaining tool families and production wiring

Review/select dynamic tools, WebSearch, FetchURL, GitHub remainder, and every remaining v2 `agent/tools` source unit. Resolve candidate-only wrappers by connecting the real path or documenting that an existing alternative is behaviorally equivalent. Avoid unrelated feature additions.

Acceptance criteria:
- No v2 source unit is omitted from the reviewed inventory.
- Every final verdict is backed by source comparison plus a test/fixture/real-path observation, or remains visibly unresolved.
- Tool schema and registration tests prove the model-visible tool table reaches the implementation that was tested.
- No tool is marked equivalent solely because a Rust module with a similar name exists.

## 6. Verify and synchronize the ledger incrementally

After each family, run its narrow tests and the relevant checks that can execute; inspect the exact compiler/test error set if the shared harness fails. Update only the reviewed rows and append batch evidence in `reports/runbooks/v2-parity-ledger.md`. Do not refresh baselines or use `--update` in parallel.

Acceptance criteria:
- Every executed check has a recorded command, real exit code, and a clear scope.
- If the test harness is blocked, the block is shown as unverified and attributed to the observed unrelated errors; no behavioral success is claimed from `cargo check` alone.
- The ledger contains v2 source locations, Rust production paths, tests run, remaining divergences, and all behavior not exercised.
- The worktree diff is reviewed and unrelated/in-flight changes remain intact; no commit is made without a separate request.
