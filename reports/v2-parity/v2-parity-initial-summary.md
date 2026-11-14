# v2 parity 初始清单汇总

- 数据时点（UTC）：2026-11-11T17:18:10.121Z
- v2：`D:/kimi/kimi-code`，commit `b07b21319733349c63e5284f2b2df91d77231cc8`
- 范围：v2 全部 src `.ts`/`.tsx`，排除 test 与 `.d.ts`；agent 288 条沿用既有判定，其余为机器初判
- 重要：`unported` 只表示“没有经过验证的 Rust 对应实现”，`candidateRustPaths` 只是路径/符号/协议名候选，**不是等价**；本 baseline 是未经核对的初值

## 口径选择

选择 **全部 src 2,569 文件 / 472,912 行**，而不是只做引擎 1,903 文件：

- 前端 452 文件 / 115,405 行属于迁移范围，只是形态待定；用 `ui` 单列后仍应进入总账，否则会隐藏 24.4% 的 src 行数。
- apps 非 UI 的 214 文件 / 42,737 行（CLI、VS Code 宿主、vis server）同样从未判定过，不能留在账外。
- 测试 1,073 文件 / 432,085 行不进入文件级清单：Rust 测试与 TS 测试不是一一对应，逐文件记账会产生虚假覆盖率；测试等价另用行为用例清单。
- `agent-core-v2/src/agent` 的 288 条不复制、不改写，由清单的 `inherit` 指向现有 `scripts/agent-parity-manifest.json`，判定值保持原样。

## 汇总

| 分类 | 文件 | 物理行 |
|---|---:|---:|
| equivalent | 216 | 31,147 |
| partial | 51 | 13,085 |
| different | 0 | 0 |
| missing | 0 | 0 |
| unported | 1,829 | 312,036 |
| planned | 0 | 0 |
| ui | 452 | 115,405 |
| na | 21 | 1,239 |
| **合计** | **2,569** | **472,912** |

已验证等价：216 文件 / 31,147 行；未完成（partial + unported）：1880 文件 / 325,121 行；UI 待定形态：452 文件 / 115,405 行。

机器初判：2569 行中有 504 行带候选落点；1,829 条 unported 中 504 条有候选（27.6%），1325 条没有候选。候选全部未核，不能当作等价。`planned` 为 0：当前没有机器可读的排期清单，不把“好像有计划”当作 planned。

## 按包

| 包/单元 | 文件 | 行 | equivalent | partial | unported | planned | ui | na | 候选数 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| agent-core-v2 | 1349 | 190,074 | 216 / 31,147 | 51 / 13,085 | 1,061 / 144,603 | 0 / 0 | 0 / 0 | 21 / 1,239 | 313 |
| apps/kimi-code | 425 | 103,822 | 0 / 0 | 0 / 0 | 145 / 29,550 | 0 / 0 | 280 / 74,272 | 0 / 0 | 22 |
| kap-server | 172 | 32,939 | 0 / 0 | 0 / 0 | 172 / 32,939 | 0 / 0 | 0 / 0 | 0 / 0 | 42 |
| minidb | 62 | 20,910 | 0 / 0 | 0 / 0 | 62 / 20,910 | 0 / 0 | 0 / 0 | 0 / 0 | 7 |
| pi-tui | 45 | 20,821 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 45 / 20,821 | 0 / 0 | 0 |
| node-sdk | 51 | 13,527 | 0 / 0 | 0 / 0 | 51 / 13,527 | 0 / 0 | 0 / 0 | 0 / 0 | 22 |
| apps/vscode/webview-ui | 74 | 10,988 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 74 / 10,988 | 0 / 0 | 0 |
| kosong | 36 | 10,420 | 0 / 0 | 0 / 0 | 36 / 10,420 | 0 / 0 | 0 / 0 | 0 / 0 | 16 |
| apps/vis/web | 53 | 9,324 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 | 53 / 9,324 | 0 / 0 | 0 |
| apps/vscode | 41 | 8,997 | 0 / 0 | 0 / 0 | 41 / 8,997 | 0 / 0 | 0 / 0 | 0 / 0 | 5 |
| oauth | 33 | 8,459 | 0 / 0 | 0 / 0 | 33 / 8,459 | 0 / 0 | 0 / 0 | 0 / 0 | 10 |
| klient | 51 | 6,406 | 0 / 0 | 0 / 0 | 51 / 6,406 | 0 / 0 | 0 / 0 | 0 / 0 | 22 |
| acp-server | 26 | 5,904 | 0 / 0 | 0 / 0 | 26 / 5,904 | 0 / 0 | 0 / 0 | 0 / 0 | 7 |
| tree-sitter-bash | 7 | 5,325 | 0 / 0 | 0 / 0 | 7 / 5,325 | 0 / 0 | 0 / 0 | 0 / 0 | 1 |
| transcript | 27 | 4,815 | 0 / 0 | 0 / 0 | 27 / 4,815 | 0 / 0 | 0 / 0 | 0 / 0 | 14 |
| migration-legacy | 33 | 4,312 | 0 / 0 | 0 / 0 | 33 / 4,312 | 0 / 0 | 0 / 0 | 0 / 0 | 4 |
| apps/vis/server | 28 | 4,190 | 0 / 0 | 0 / 0 | 28 / 4,190 | 0 / 0 | 0 / 0 | 0 / 0 | 9 |
| session-import | 10 | 3,865 | 0 / 0 | 0 / 0 | 10 / 3,865 | 0 / 0 | 0 / 0 | 0 / 0 | 2 |
| remote-control | 5 | 2,279 | 0 / 0 | 0 / 0 | 5 / 2,279 | 0 / 0 | 0 / 0 | 0 / 0 | 4 |
| product | 16 | 1,924 | 0 / 0 | 0 / 0 | 16 / 1,924 | 0 / 0 | 0 / 0 | 0 / 0 | 1 |
| i18n | 4 | 1,677 | 0 / 0 | 0 / 0 | 4 / 1,677 | 0 / 0 | 0 / 0 | 0 / 0 | 1 |
| telemetry | 10 | 1,423 | 0 / 0 | 0 / 0 | 10 / 1,423 | 0 / 0 | 0 / 0 | 0 / 0 | 2 |
| i18n-shared | 5 | 439 | 0 / 0 | 0 / 0 | 5 / 439 | 0 / 0 | 0 / 0 | 0 / 0 | 0 |
| runtime-shared | 6 | 72 | 0 / 0 | 0 / 0 | 6 / 72 | 0 / 0 | 0 / 0 | 0 / 0 | 0 |
| **合计** | **2569** | **472,912** | **216 / 31,147** | **51 / 13,085** | **1,829 / 312,036** | **0 / 0** | **452 / 115,405** | **21 / 1,239** | **504** |

## 除 agent 288 条外疑似缺失 Top 30（按行数）

排除 `agent-core-v2/src/agent` 的既有 288 条；同一包的其余 1,061 条仍计入。候选落点未核，不能当作等价。

| # | 行 | 包 | 文件 | 候选落点（未核） |
|---:|---:|---|---|---|
| 1 | 3,997 | tree-sitter-bash | parser.ts | — |
| 2 | 3,522 | agent-core-v2 | features/tower/protocol/store.ts | packages/kimi-agent/src/tools/tower/store.rs (+2) |
| 3 | 3,461 | apps/kimi-code | i18n/locales/en.ts | — |
| 4 | 3,340 | apps/kimi-code | i18n/locales/zh.ts | — |
| 5 | 3,337 | node-sdk | sdk-rpc-client-v2.ts | — |
| 6 | 2,082 | agent-core-v2 | workspace/workspaceFs/fsService.ts | — |
| 7 | 1,811 | agent-core-v2 | app/telemetry/events.ts | — |
| 8 | 1,692 | agent-core-v2 | features/goal/goalService.ts | — |
| 9 | 1,654 | kap-server | services/transcript/coreEventMap.ts | — |
| 10 | 1,600 | minidb | mini-db.ts | — |
| 11 | 1,594 | kap-server | transport/ws/v1/sessionEventBroadcaster.ts | packages/kimi-agent/src/server/ws_protocol.rs |
| 12 | 1,553 | kap-server | search/indexCore.ts | — |
| 13 | 1,502 | agent-core-v2 | features/associativeMemory/service.ts | — |
| 14 | 1,493 | remote-control | remote-control.ts | packages/kimi-agent/src/server/remote_control.rs |
| 15 | 1,424 | kap-server | routes/sessions.ts | packages/kimi-agent/src/server/idle_sessions.rs (+1) |
| 16 | 1,413 | apps/kimi-code | cli/sub/provider.ts | — |
| 17 | 1,391 | kosong | providers/anthropic.ts | packages/kimi-agent/src/llm/anthropic.rs |
| 18 | 1,288 | kosong | providers/openai-responses.ts | packages/kimi-agent/src/llm/openai_responses.rs (+1) |
| 19 | 1,269 | node-sdk | errors.ts | packages/kimi-core/src/primitives/errors/serialize.rs |
| 20 | 1,268 | apps/kimi-code | cli/v2/run-v2-print.ts | — |
| 21 | 1,242 | kap-server | protocol/events-zod.ts | packages/kimi-agent/src/server/web_events.rs |
| 22 | 1,234 | session-import | rollout.ts | — |
| 23 | 1,195 | acp-server | session.ts | packages/kimi-agent/src/napi_bindings/session.rs (+1) |
| 24 | 1,166 | minidb | gen-codec.ts | — |
| 25 | 1,111 | agent-core-v2 | mcpCore/connection-manager.ts | packages/kimi-agent/src/mcp/manager.rs |
| 26 | 1,101 | kosong | providers/google-genai.ts | packages/kimi-agent/src/llm/google_genai.rs |
| 27 | 1,100 | agent-core-v2 | workspace/sessionLifecycle/sessionLifecycleService.ts | — |
| 28 | 1,079 | kap-server | routes/fs.ts | packages/kimi-agent/src/server/route_workspaces_fs.rs |
| 29 | 1,078 | session-import | claude-code.ts | — |
| 30 | 1,051 | apps/kimi-code | cli/update/preflight.ts | — |

## 口径与限制

- 覆盖 19 个 packages 的 `src`，以及 `apps/vis/web`、`apps/vis/server`、`apps/vscode/webview-ui`、`apps/vscode/src`、`apps/vscode/shared`、`apps/kimi-code/src`、`apps/kimi-code/src/tui`。
- 测试 1,073 文件 / 432,085 行未进入文件级清单：Rust 测试与 TS 测试不是一一对应，逐文件台账会产生虚假覆盖率。
- 前端 UI 统一为 `ui`，迁移单位应是屏幕/交互流程/协议，不是文件；`pi-tui` 按 UI 基础设施处理。
- 机器匹配只索引 Rust `pub` 符号、模块路径和协议字符串；匹配到只写 `candidateRustPaths`，`rustPaths` 保持空，`evidence` 保持空，等待人工核对。
- 候选阈值：文件 stem 精确匹配、路径连续 ≥3 段、或命中 Rust `pub` 符号且总分 ≥6，或总分 ≥8；每个 TS 文件最多列 3 个候选。
- 候选不是“确认缺失”的反面：没有候选也不能证明 Rust 侧一定没有实现，只表示当前机器规则对不上；因此全部保持 `unported`。
- baseline 是当前状态的快照，不代表已验收；任何新文件未登记都会红灯。

## 产物与命令

- 清单：`scripts/v2-parity-manifest.json`（26 个包/单元，2,281 条机器行 + 288 条继承的 agent 行）
- 基线：`scripts/v2-parity-baseline/<unit>.json`，每包/单元一份，`kind` 为 `unverified-initial`
- 重新生成机器行：`bun run generate:v2-parity`（`reviewed:true`、非 `unported` 判定或非空 `evidence` 的行会保留，不会被覆盖）
- 检查：`bun run check:v2-parity`
- 显式更新基线：`bun run check:v2-parity:update`
- 旧 agent 门禁仍保留：`bun run check:agent-parity` / `bun run check:agent-parity:update`；v2 门禁通过 `inherit` 读取同一份 288 条判定，不复制、不改写。
