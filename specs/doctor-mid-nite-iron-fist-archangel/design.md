# Design: v2 `agent/tools` Behavioral Alignment

## Source of truth

Use `D:/kimi/kimi-code/packages/agent-core-v2/src/agent/tools` as the user-maintained behavioral reference. Use `.tmp/v2-ref-upstream` as a separate upstream comparison. When these references differ, preserve the conflict as an explicit unresolved item; do not silently choose one. The existing `reports/runbooks/v2-parity-ledger.md` is the incremental evidence ledger, not a substitute for reading the reference implementation.

## Execution path and ownership

For each v2 tool file, record the Rust module(s), behavior owner, registration source, and runtime dispatch route. Read `packages/kimi-agent/src/tools/builtin_assembly.rs`, the relevant tool registration/services, and `packages/kimi-agent/src/tools/dispatch.rs` before deciding that a `packages/kimi-agent/src/agent/tools/**` wrapper is production behavior. Some v2-shaped wrappers exist beside an older engine path; parity work must fix or route the path the engine actually executes rather than make an unused wrapper look correct.

Keep each batch within one behavior family and its necessary direct consumers. Expected family groupings are: question/task/agent execution; filesystem and media; shell/PowerShell/process output; GitHub requests and result shaping; URL/web search; dynamic tool selection. This is a work decomposition, not an assumption that every family is independently wired.

## Behavioral comparison

For every in-scope unit, compare the v2 code and Rust production route across the applicable dimensions:

- advertised schema, defaults, aliases, unknown fields, and validation diagnostics;
- side-effect/repository-mutation and resource-access declarations;
- cancellation, timeout, detach, wait, and cleanup semantics;
- response projection, ordering, truncation, spill, status, and metadata fields;
- feature/config/permission gates and actual registration/dispatch behavior.

Do not invent behavior based on type similarity. When the Rust engine implements the same observable contract through a different mechanism, document that evidence and test the observable boundary.

## Verification design

Add or extend tests in existing Rust test modules for the behavior under change. Tests should drive the real registration/dispatch path where available and use local deterministic fakes or loopback services for external-service protocols. A pure helper test is supplementary and cannot establish production wiring. For each batch, record the exact test/gate command and exit code. Run the narrowest check that can observe the batch; keep the lib-test-harness blocker separate from behavioral conclusions. Do not weaken or skip tests to get green.

## GitHub behavior slice already in progress

The current unverified local change adds per-spec `max_bytes`, captures `Link` `rel="next"`, and exposes `next_page` / `truncated_bytes`. Its PR diff limit is incorrectly set to 200 * 1024 bytes; v2 `github-tools.ts:809` specifies exactly 200_000 bytes. Correct that constant and its tests before claiming this slice aligned. The current reader also drains the remainder after overflow whereas v2 `github-request.ts:263-270` cancels the reader; match cancellation and report only the observed chunk overflow. Do not append a new `(has_more: true)` suffix: v2 passes pagination to its tool-specific projections. The next review must verify the local response behavior against v2's `readResponseBody` semantics, including the truncation marker and stopping/closing behavior, and must confirm that page metadata reaches the caller in the shape v2 uses. If returning a legacy text result cannot represent the v2 shape, record that difference and adjust the dispatch adapter in the same batch rather than claiming parity from the request layer alone. Preserve the current user-in-flight GitHub validation/cancellation edits.

## Rejected approaches

- **Treat successful compilation as parity:** rejected because the module can be syntactically wired while observable request, cancellation, or result behavior still differs.
- **Port or fix only files named `agent/tools` without tracing registration and dispatch:** rejected because it can leave the true production route unchanged.
- **Bulk-migrate all tools in one edit:** rejected because the blast radius and shared interface dependencies would prevent trustworthy behavioral attribution.
- **Rebuild or wholesale refresh the parity ledger:** rejected because it is a cumulative artifact and the worktree contains concurrent edits; update only reviewed rows incrementally.

## Constraints

Follow repository-wide development and ledger rules from `AGENTS.md` and `reports/runbooks/dev-workflow.md`; do not copy those global rules into the ledger. The current Rust tree has unrelated compiler errors, so a passing package test suite may be unavailable until those blockers are resolved. No user changes may be discarded to make a baseline.
