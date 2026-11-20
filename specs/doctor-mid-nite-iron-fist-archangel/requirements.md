# Requirements: v2 `agent/tools` Behavioral Alignment

## Goal

Bring the Rust engine's implementation of the v2 `packages/agent-core-v2/src/agent/tools` module into behavioral alignment with the user-maintained v2 reference, while preserving the Rust engine's existing architecture where it already produces the same externally observable behavior. The completion target is behavioral parity, not a lower compiler-error count or a compilation-only port.

## Audience and outcomes

This work is for maintainers of the Rust engine. For each covered tool, maintainers should be able to trace the v2 behavior to its Rust implementation and exercise the actual tool path that advertises and dispatches it.

## Scope

- Inventory every v2 source unit under `agent/tools` and map it to the Rust implementation, any existing older implementation, and the production registration/dispatch path.
- Compare and align each tool's input contract, validation/error shape, permission and side-effect metadata, cancellation behavior, output/result shape, task lifecycle, and caller-visible auxiliary fields where that behavior is present in v2.
- Cover tool families including shell/process tasks, filesystem read/write/search, GitHub, URL fetch and web search, media reads, user questions, task tools, subagent/agent tools, and dynamic tool selection.
- Where v2 contains helper modules used by these tools (for example GitHub result shaping), include their behavior in the owning tool-family batch.
- Preserve existing in-flight user/worktree changes. Before taking over a changed file, establish whether the edit belongs to the requested behavior and retain unrelated changes.
- Record each completed behavior slice and each remaining divergence or unverified boundary in the existing `reports/runbooks/v2-parity-ledger.md`, with exact v2 evidence, Rust production path, test evidence, and truthful verification status.

## Acceptance

The module is complete only when every in-scope v2 source unit has a reviewed disposition and every behavior-bearing unit is either:

1. covered by an executable Rust implementation that is wired into the actual engine registration/dispatch path and has focused behavioral evidence; or
2. explicitly identified as a semantic adaptation with evidence that the externally observable behavior matches v2; or
3. recorded as a remaining, unresolved divergence, in which case the module is not reported complete.

A compiling crate, a candidate wrapper, a manifest verdict, or a fall in the global error count alone is not evidence of behavioral alignment. Each batch must exercise the relevant production route, including the dispatch seam, for the behavior being claimed.

## Out of scope

- Aligning tools outside v2 `agent/tools` except for the minimum direct caller or shared contract changes needed to realize an in-scope behavior.
- Broadly porting other v2 subsystems such as the turn loop, task service, permission system, workspace layer, or provider stack. A newly discovered dependency may be documented as a blocker or proposed as a separate follow-up rather than silently expanding this migration.
- Reverting, rebasing, committing, pushing, or refreshing whole-workspace ledgers/baselines without separate authorization.
- Claiming full test or build success while unrelated existing compiler errors prevent the relevant harness from running.
