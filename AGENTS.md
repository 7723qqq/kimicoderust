# Repository-level Agent Guide

Reply in the same language as the user.

This is a TypeScript monorepo built for agent-assisted development. This file holds the rules that apply to every task. The detailed reference — project map, commands, CI, stack, the full upstream merge policy, the i18n rules, and the full verification standard — lives in `reports/repo-guide.md`; read it when a task needs the map, the commands, or the background. The project's standing local references (the user's fork at `D:\kimi\kimi-code` and the Bun 1.4.4 source at `D:\kimi\bun`) are recorded in `MEMORY.md`; use them without asking again.

> **Merging, porting, or triaging upstream changes? Read
> [`reports/repo-guide.md#upstream-merge-policy`](reports/repo-guide.md#upstream-merge-policy)
> before judging anything.** `packages/kimi-agent` is a **port of v2
> (`agent-core-v2`)**, not an independent implementation, so an upstream change under
> `packages/agent-core-v2/**` is a **behavior spec the Rust engine must follow** —
> *even though those files no longer exist in this fork*. The judgement criterion is
> the v2 → Rust module mapping, **never** "the path is absent from `HEAD`, so it is
> irrelevant". `bun run check:upstream-v2-delta` is the mechanical gate — **refresh
> `refs/remotes/upstream/main` before you trust it**. Upstream merges are made by
> hand: never `git pull` / `git merge` against upstream.

## 开发行为规范（强制）

以下条款人与模型均须遵守；事故依据、操作步骤和验收方法见对应细则。

- 累积型台账与配置禁止整文件回滚或覆盖，只能逐行编辑，或先备份再做可审计的增量修改。（[细则](reports/runbooks/dev-workflow.md#ledger-no-rollback)）
- 所有 `--update` 与指纹刷新必须由单一会话在并行写入停止后执行，并留存前后快照与差异核对。（[细则](reports/runbooks/dev-workflow.md#single-writer-updates)）
- 并行窗口开工前必须核对文件占用，接手半截改动必须先判定可继承状态，未明确交接不得修改或默认回滚。（[细则](reports/runbooks/dev-workflow.md#file-ownership)）
- 验证按最窄静态门禁与定向测试分层，全量回归只在交付前验收，失败必须先做基线对照和窄化复现。（[细则](reports/runbooks/dev-workflow.md#verification-tiers)）
- 批次只有完成源码核实、真实出口码、台账同步、无半截状态且未经授权不提交 commit 才算收口。（[细则](reports/runbooks/dev-workflow.md#batch-closure)）
- 上游 delta 必须回到 `upstream/main` 源码逐 sha 核实并同步对应台账，禁止用 note 代替证据或全量刷新覆盖判定。（[细则](reports/runbooks/dev-workflow.md#upstream-delta)）
- 大文件拆分只能纯搬运，项级与测试名对账必须无缺失无多余，适配仅限模块层级和最小可见性。（[细则](reports/runbooks/dev-workflow.md#pure-moves)）
- 补丁代码必须按 `packages/**/src/**/patches/**` 路径界定，代码行占第一方源码行不得超过 0.05%（当前基线约 104 行）；补丁仅限例外而非常规手段，禁止用注释标记或调整统计口径规避。（[细则](reports/runbooks/dev-workflow.md#patch-budget)）
- 门禁绿灯只代表其实际覆盖面，发现截断、跳过或漏收必须另立盲区条目，禁止扩大豁免换绿。（[细则](reports/runbooks/dev-workflow.md#gate-blindspots)）
- 文档中的当前统计数字必须现读权威文件或命令输出并注明口径，禁止手写不可复核的快照。（[细则](reports/runbooks/dev-workflow.md#live-counts)）
- PowerShell 门禁必须直跑并读取 `$LASTEXITCODE`，不得用管道或后台 shell 状态替代真实出口码。（[细则](reports/runbooks/dev-workflow.md#exit-codes)）

## Hard constraints

- **Bun is the only toolchain.** `>= 1.4`; no Node floor in any published package. On an upstream merge, refuse Node-side content (`pnpm-lock.yaml`, `engines.node`, Node-only scripts, SEA build steps, Node-installing CI jobs) and re-express the need on Bun. The one exception is `packages/node-sdk`, which is a product SDK name, not toolchain.
- **Versions follow `upstream/main`.** Never run `bun run version` / `bun run publish`; never bump independently. Changesets are a changelog source only.
- **Do not commit unless the user asks.** When asked, push to `origin` immediately after committing; never let local and remote diverge. No co-author attribution, and do not reveal the agent's identity in commits, PR descriptions, or any explanatory text.
- **The Rust engine is a port, not a design surface.** Every behavior it implements needs a traceable reference or an explicit user ruling; anything else is a defect, not a design choice.
- **Locale catalog:** `packages/i18n-catalog` is the single owner of all `en` / `zh` keys; `packages/kimi-agent/src/locales/*.json` is generated from it. Run `bun run check:engine-i18n` after any change.
- **Verify before claiming done.** Drive the real path — the real engine, host seam, and config — and observe the outcome. A green suite is a *possible* result, not the target.

## Verification tiers

Pick the narrowest tier that can observe the change. The full normative standard — including the v2 reference checkouts (`.tmp/v2-ref`, `.tmp/v2-ref-upstream`) and the known load-dependent flakes — is in [`reports/repo-guide.md#verification-standard-normative`](reports/repo-guide.md#verification-standard-normative).

| Tier | Use after | Cost |
| --- | --- | --- |
| **1 · fast** | one function / branch | seconds |
| **2 · module** | a module, service, or route | ~1 min |
| **3 · package** | a shared type, contract, or schema | minutes |
| **4 · full** | before delivery only | tens of min |

Rule of thumb: `cargo test --lib <module>` for one Rust module; the package's own tests plus the `check:*` gates its diff can trip for a module; the full suite, `typecheck`, `lint`, and `build` only before delivery. Do not re-run the full suite to confirm a fix — fix the failing item and re-run that item.

## Workflow requirements

- Use the `write-pr` skill (`.agents/skills/write-pr/SKILL.md`) when creating a PR, and run the `gen-changesets` skill (`.agents/skills/gen-changesets/SKILL.md`) before submitting one. Never decide a `major` bump on your own — stop and ask; default to `minor`.
- In public text and test data, replace real internal identifiers with neutral placeholders (`example.com`, `example.test`, `YOUR_API_KEY`).
- Do not commit throwaway scratch or exploratory files (handoff notes, design mockups). Put scratch work under `.tmp/` (gitignored).
- Keep changes focused; do not slip in unrelated refactors. Ship complete implementations — a stub, a no-op body, or a test asserting a behaviour nothing implements is a defect, not a placeholder.

## Where to Update Instructions

- Rules that affect almost every task: this file.
- Rules that only affect a specific directory: the nearest sub-directory `AGENTS.md`.
- The project map, command reference, CI description, and background: `reports/repo-guide.md`.
- Keep instruction updates focused and supported by code facts.
