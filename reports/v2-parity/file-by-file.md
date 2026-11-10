# 逐文件对齐台账（v2 → Rust 引擎）

## 元信息

| 项 | 值 |
| --- | --- |
| 目标仓库 | `g:\kimirust`，`packages/kimi-agent/src` |
| 参考实现 | `D:\kimi\kimi-code` 工作区 `packages/agent-core-v2/src`（HEAD `d4c849b2da`） |
| 口径 | 以 v2 目录为起点逐文件读源码，找 Rust 落点；判定「等价 / 部分 / 缺失 / NA」，只补引擎内可闭合项 |
| 约束 | 不改 `packages/protocol`、`packages/node-sdk`、`apps/kimi-code`；台账仅按行登记，不跑 `--update` |
| 起始日期 | 2026-11-09 |

判定口径：**等价**＝可观察行为一致（含换位实现）；**部分**＝主路径一致但字段/分支缺失；**缺失**＝v2 有、Rust 无；**NA**＝引擎无对应职责或 v2 本身无该行为（写理由）。

---

## 第一批：`agent/loop/`（14 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `configSection.ts` | `config/mod.rs` `[loop_control]` | 等价 | 6 个键、3 个 env、legacy 别名与弃用映射均一致（`resolve_max_attempts_per_step` / `resolve_max_steps_per_turn`） |
| `errors.ts` | `session/mod.rs` `turn_end_error_payload`；`agent_busy` 无落点 | 部分 | `loop.max_steps_exceeded` 文案逐字一致；`turn.agent_busy` 属 v2 程序化 skill 服务，Rust 引擎无该服务 → NA |
| `loop.ts` | `turn_loop/types.rs`、`session/mod.rs`、`server/mod.rs` | 等价 | `TurnResult`/`LoopSnapshot`/prompt 状态均有等价面；`registerLoopErrorHandler` 唯一消费者是 fullCompaction，溢出恢复已直接实现 → NA |
| `loopService.ts` | `turn_loop/run_turn.rs`、`session/mod.rs` | 部分 | B1 已对 62 条判定；本批补 `turn.ended` 的 `interruptReason`/`traceId`。剩余：`prompt.queued`/`prompt.started`（协议无事件，跨层）、`stopReason`（协议无字段，跨层） |
| `promptChannel.ts` | `session/mod.rs` 的 `Admission` 四种准入 + `server/prompt_queue.rs` | 等价 | `ActiveOrNewTurn`/`ActiveTurnOnly` 覆盖 steer 判定；prompt metadata 由 `server/engine.rs::apply_prompt_metadata` 写回 |
| `turnEvents.ts` | `turn_events.rs` + `events/types.rs` | 部分→已补 | 本批补齐 `turn.ended.interruptReason`/`traceId`；修正 bundled skill 显示文本派生（v2 `annotateBundledSkillParts` 语义） |
| `turnOps.ts` | `turn_events.rs`、`storage/state_store.rs::fold_turn_event` | 等价 | `TurnPrompt`/`TurnSteer`/`TurnCancel`/`TurnEnded` 载荷与 `turnKey` 折叠（nextTurnId / cancelledTurnIds / anchorTurnIds / lastEnded）逐项一致 |
| `machine/engine.ts` | `turn_loop/run_turn.rs` + `turn_step.rs` + `tool_scheduler.rs` | 等价 | 状态机行为已直接实现（B1 已判）；事件词表用 `llm.step.*` 而非 `turn.step.*`，信息面等价 |
| `machine/history.ts` | 无独立落点 | NA | v2 human 机器的 HistoryMessage 元数据层；Rust 直接维护 `LLMMessage` 历史 |
| `machine/index.ts` | `session/mod.rs::TurnRequest` | 等价 | `createUserEntry` 对应 `TurnRequest::user` |
| `machine/requester.ts` | `turn_step.rs` + `llm/**` | 等价 | gate 失败/取消 → 失败且不可重试；`llm.sent`、retrying、usage/finish/message_id 均有对应事件 |
| `machine/storeJournal.ts` | `native/event_store/` + `session/sqlite_store.rs` | NA | Rust 用 SQLite 行日志，不采用 v2 的 human store journal 适配层 |
| `machine/tools.ts` | `turn_loop/tool_scheduler.rs` | 等价（除下述） | 工具机器适配已由调度器直接实现；`stopBatchAfterThis` 见「已识别未补」 |

## 第一批：`agent/toolExecutor/`（6 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `toolExecutor.ts` | `turn_loop/tool_scheduler.rs`、`tools/dispatch.rs` | 等价 | 执行入口与结果形状对应 |
| `toolExecutorService.ts` | `turn_loop/tool_scheduler.rs`、`tools/dispatch.rs`、`tools/file_ops.rs` | 部分 | abort grace 2s、skipped 文案、aborted 输出、preflight/resolution/veto/synthetic 判定均已实现；`stopBatchAfterThis` 未实现（见下） |
| `toolExecutorEvents.ts` | `events/types.rs`、`turn_loop/run_turn.rs`、`napi_bindings/mcp.rs` | 部分→已补 | `tool.call.started`（含 description/display）、`tool.progress`、`tool.call.delta` 已有；本批补发 `tool.list.updated`（协议已声明，引擎此前从不发） |
| `beforeToolExecuteEvent.ts` | `tools/before_execute.rs` | 等价 | veto/allow/pass/waitUntil 语义一致（B5 已判） |
| `toolHooks.ts` | 直接调用点 | NA | hook 缝被引擎内直接实现替代（checkpoint / progress / AGENTS.md / testScopeGuard / dedupe / goal） |
| `toolScheduler.ts` | `turn_loop/tool_scheduler.rs` | 等价 | 访问冲突调度、完成后启动排队任务一致 |

## 第一批：`agent/toolPolicy/`（4 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `configSection.ts` | `config/mod.rs` `[tools]` | 等价 | enabled/disabled |
| `evaluate.ts` | `tools/tool_policy.rs` | 等价 | `isToolActive` / `isToolActiveComposed` / `resolveActiveToolNames` / `literalToolNames` / `findInactiveToolPatterns` 均有对应；内置工具大小写不敏感为已文档化的有意差异 |
| `toolPolicy.ts` | `tools/tool_policy.rs`、`tools/select_tools.rs` | 等价 | 指纹计算见 `select_tools.rs` 文档；会话禁用工具已有持久化 |
| `toolPolicyService.ts` | `tools/dispatch.rs` 的 `blocks_call` 守卫 | 部分 | 拒绝文案用本地化 `engine.tools.disabled`，v2 原文 `Tool "X" is disabled by the active tool policy`；文案差异待批 |

## 第一批：`agent/toolApproval/`（2 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `toolApproval.ts` | `permission/mod.rs` | 等价 | 审批请求/结果通道由权限链承载 |
| `toolApprovalService.ts` | `callbacks.rs`（审批请求）、`permission/mod.rs` | 等价 | 拒绝/取消文案、worker 额外指引、session 级 approve 规则记录均已实现（B5 权限链 15 条） |

## 第一批：`agent/toolResultTruncation/`（2 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `toolResultTruncation.ts` | `tool_result_truncation.rs` | 等价 | 常量、指针块、preview 头尾、行内截断一致 |
| `toolResultTruncationService.ts` | `tool_result_truncation.rs` | 部分 | spill 目录为 `<workspace>/.kimi/spill`（v2 为 `<home>/<agent scope>`），路径可见但属宿主差异；`spillExempt` 在调用点实现 |

---

## 本批改动文件

| 文件 | 改动 |
| --- | --- |
| `src/turn_events.rs` | 新增 `TurnInterruptReason`；`TurnEnded` 增加 `interruptReason`/`traceId`；`display_prompt` 实现 bundled skill 位置标注；新增 5 条测试 |
| `src/session/mod.rs` | `end_reason_of` 旁新增 `interrupt_reason_of`；两处 `TurnEvent::Ended` 填充 interruptReason/traceId |
| `src/session/tests.rs` | `turn.ended` 断言同步 |
| `src/storage/state_store.rs` | 测试构造同步 |
| `src/server/activity.rs` | 测试构造同步 |
| `src/napi_bindings/mcp.rs` | 新增 `tool.list.updated` 发射（connected/needs-auth → mcp.connected；connected→disabled → mcp.disconnected），附 2 条单测 |

## 结论一：本批已闭合

- `turn.ended` 的 `interruptReason`（`aborted` / `max_steps` / `filtered` / `error`；干净结束与 blocked 不带）与 `traceId`（引擎铸 `turn-<id>`，与 telemetry 同源）。
- bundled skill activation 的显示文本：宿主把 skill 块前置并只在 `origin.skillActivations` 里登记时，`turn.started.prompt` 不再泄漏 skill 正文。
- `tool.list.updated`：MCP 服务器工具面变化时可被宿主感知（协议与 ws 契约早已声明，此前引擎从不发）。

## 结论二：已识别、且已在本批补齐

| # | 缺口 | 落点 | 验证 |
| --- | --- | --- | --- |
| 1 | `stopBatchAfterThis`（UpdateGoal / SetGoalBudget 的预判跳过） | `tools/goal_tools.rs::predicted_stop_batch`（执行前读 goal 快照并按 v2 规则判定）+ `turn_loop/tool_scheduler.rs::execute_scheduled` 新增 `run_count`，越过停止点的调用不发车、直接落 `SKIPPED_TOOL_OUTPUT`；`run_turn.rs` 传入预判位置并对越界位置强制 skipped（含 preflight 本会拒绝的调用，与 v2 `prepareSkippedToolCall` 一致） | `cargo test --lib tool_scheduler` 40 通过（含新增 `test_execute_scheduled_predicted_stop_skips_the_tail`）；`cargo test --lib tools::goal_tools` 24 通过（含新增 `test_stop_batch_prediction_matches_v2s_resolve_execution`） |
| 2 | HTTP/WS 主机的 MCP 状态事件缺失 | 新增共享模块 `mcp/status.rs`（`mcp.server.status` + `tool.list.updated` 映射、订阅与 replay、`McpStatusBridge`）；napi 仅保留 sink 包装；`ServerEngine` 按会话持有 bridge（`ensure_mcp_status_bridge` / `forget_mcp_status`），事件发布到该会话 lane；删除/归档/ACP dispose 时释放订阅 | `cargo check --lib`、`cargo check --features cli` 通过；`cargo test --lib mcp::status` 2 通过 |
| 3 | 工具禁用文案 | `src/locales/{en,zh}.json` 与 `packages/i18n-catalog/src/locales/{en,zh}.ts` 对齐 v2 原文 `Tool "<name>" is disabled by the active tool policy` | `check:engine-i18n` 通过（223 keys） |

## 结论三：跨层项（已获授权并补齐）

| # | 缺口 | 落点 | 验证 |
| --- | --- | --- | --- |
| 1 | `turn.ended.stopReason` | `packages/protocol/src/events.ts` 增 `stopReason?: string` + schema；引擎 `TurnEvent::Ended` 增加字段，`session/mod.rs::forced_stop_reason_of` 只在 repeat breaker 时填 `repeat_breaker`（v2 `loopService.ts:2217`） | `cargo test --lib turn_events` 13 通过（含 `stopReason` 往返断言） |
| 2 | `traceId` / `interruptReason` 的宿主消费 | `packages/node-sdk/src/native/sdk-client/build-handle.ts` 的 `turn.ended` 映射补齐 `traceId`（`traceId ?? trace_id`）、`interruptReason`、`stopReason` | protocol typecheck 通过；node-sdk typecheck 仅剩 `i18n-catalog` 的 `backgroundStartRefused` 缺 zh 键（另一窗口在途，非本批） |
| 3 | `prompt.queued` / `prompt.started` | `packages/protocol` 增事件类型、union 与 schema；引擎 `builder.rs` 在入队时发 `prompt.queued`（含 `queueLength`、去掉 bundled skill 块的内容），`run_prompt_loop` 在每次用户 prompt 开跑前发 `prompt.started`；`PromptQueue::queued_count`；`ws-event-contract.json` 登记；transcript 投影新增两个 fold（queued → `Queued`，started → `Running`）；`NON_PROJECTED_EVENT_TYPES` 登记避免回放误报未知事件 | `cargo test --lib server::prompt_queue` 19 通过；`cargo test --lib queued_and_started_fold_the_prompt_status` 通过；`bun run check:parity` 通过 |

## 局限说明

- 行号与常量值以本批读到的源码为准，其他并发窗口的在途改动可能造成漂移。
- `cargo test --lib` 全量未在本批执行（用户规则：全量回归仅作提交前验收）；本批跑 `cargo check --lib`、`cargo check --features cli`、`cargo test --lib turn_events`、`cargo test --lib tool_scheduler`、`cargo test --lib tools::goal_tools`、`cargo test --lib mcp::status`、`cargo test --lib server::prompt_queue`、投影新增用例，以及 `check:engine-i18n`、`check:parity`、触碰文件 `rustfmt`。
- 本批经用户授权修改了 `packages/protocol`（`turn.ended.stopReason`、`prompt.queued`、`prompt.started`）与 `packages/node-sdk`（`turn.ended` 的 traceId / interruptReason / stopReason 映射）；未改 `apps/kimi-code`。
- `check:rust-module-deps` 的 1 条 `mcp -> primitives` 新增边来自其他窗口的在途改动（`mcp/http.rs`），不是本批；`scan:hardcoded:rust` 的 15 条 unrecorded / 46 条 stale 同样全部位于本批未触碰的文件。

---

## 第二批：`agent/toolRegistry` / `toolSelect` / `toolActivation` / `toolDedupe` / `agentsMdReminder` / `progressTrack`（22 文件）

### toolRegistry（4 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `toolRegistry.ts` | `tools/registry.rs` | 等价 | 接口语义由 `ToolRegistry` 承载：`register` / `list` / `listLive` / `resolve` |
| `toolRegistryService.ts` | `tools/registry.rs` | 等价 | 同名同签名重复注册为 active 且保留原 info；签名变化进 pending 且 active 表钉住旧定义；`list_live` 叠 pending；`dispose` → `unregister` |
| `toolContribution.ts` | 无独立落点 | NA | v2 的 DI contribution 注册/覆盖（`registerAgentToolService` / `overrideAgentToolService`）；引擎由 `native_tool_defs` + MCP manager 直接装配，无贡献点 |
| `builtinToolAssemblyService.ts` | 无独立落点 | NA | App 作用域的 contribution → DI 容器搬运；引擎无 DI 容器 |

### toolSelect（9 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `flag.ts` | 宿主 `RunTurnParams.tool_select` → `pipeline::RunTurnSpec.tool_select` | NA | flag 注册表在宿主；引擎只接收解析后的布尔（`KIMI_CODE_EXPERIMENTAL_TOOL_SELECT` / `[experimental].tool_select` 由宿主合并） |
| `toolRetriever.ts` | `tools/tool_retriever.rs` | 等价 | BM25 常量、停用词、token 上限、`MIN_SCORE_RATIO`、CJK bigram 一致；NFKC 归一化缺失为已记录差异 |
| `toolSelect.ts` | `tools/select_tools.rs` | 等价 | `select_tools` 名称、`LoadToolsResult` 各桶、enabled/shape/load/search 契约均有对应 |
| `toolSelectService.ts` | `tools/select_tools.rs`、`tools/tool_select_state.rs`、`tools/toolset.rs`、`tools/dispatch.rs` | 部分→已补 | `load` / `search` / `shapeTools` / `describeUnavailableTool` / `describeMissingTool` 已等价；本批补齐 `loadableToolsAnnouncement`（历史折叠 + 大目录 `<tools_relevant>` 检索）与 `shapeHistory`；`pendingLoaded` / `drainPendingToolSchemas` 由 `loaded_tools` + 逐步工具表替代；telemetry 未接 |
| `dynamicTools.ts` | `tools/tool_select_state.rs` | 部分→已补 | 本批补齐 `foldAnnouncedToolNames`、`stripDynamicToolContext`、`renderRetrievedToolsAnnouncement`、`stripStaleRetrievedToolsAnnouncements`、`latestUserQuery`；`ContextMessage.tools` 的 schema 消息无对应（`LLMMessage` 无 `tools` 字段，加载工具走逐步工具表） |
| `toolSelectAnnouncements.ts` | 无独立落点 | NA | 空接口 |
| `toolSelectAnnouncementsService.ts` | `run_turn.rs` 注入注册 + `toolset::take_disclosure_announcement` | 等价 | turn-start provider、首轮前等待 MCP 发现、返回 announcement 的路径一致；本批把 provider 变体名改为 `loadable-tools` 并传入会话历史 |
| `toolSelectSchemas.ts` | 无独立落点 | NA | 空接口 |
| `toolSelectSchemasService.ts` | `toolset::shape_tool_table` | NA | v2 用带 `tools` 的系统消息提醒 schema；引擎无该系统消息通道，以逐步工具表重新广告已加载工具（等价目标，不同通道） |

### toolActivation（3 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `toolActivation.ts` | 无独立落点 | NA | DI 服务接口 |
| `toolActivationService.ts` | `native_tool_defs` + `mcp/manager` 连接/断开 | NA | v2 按 profile / policy / environment 激活或撤销 contribution；引擎的可用性由工具定义装配与 MCP 注册/注销承担，无同形服务 |
| `toolAvailability.ts` | 各工具自身能力门（GitHub 凭据、image_in/video_in 等） | NA | v2 的 `requiredEnvironmentCapabilities` 通用门在引擎无对应抽象；需要环境的工具在各自调用点判断 |

### toolDedupe（2 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `toolDedupe.ts` | `tools/tool_dedupe.rs` | 等价 | 接口、`REPEAT_BREAKER_STOP_REASON`、结果类型 |
| `toolDedupeService.ts` | `tools/tool_dedupe.rs` + `run_turn.rs`（handoff / stopReason） | 部分 | 同一步去重、跨步 3/5/8/12 阶梯、`KIMI_CODE_REPEAT_BREAKER` 默认关、强制停后的 text-only handoff 与 veto 文案均等价（B1 已核）；未发 `tool_call_repeat` / `tool_call_dedup_detected` / `tool_call_turn_repeat` / `tool_call_repeat_handoff`（telemetry seam 未激活，已记录）；`spill.suffix` 无对应（引擎结果无 spill 字段） |

### agentsMdReminder（3 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `agentsMdReminder.ts` | `injection/mod.rs`（`AgentsMdReminder`） | 等价 | `seedInjected(paths, cwd)` 对应系统提示路径 + 历史披露扫描的种子 |
| `agentsMdReminderService.ts` | `injection/mod.rs` 的 `agents_md_provider` + `run_turn.rs` 的 `StepAccess` | 部分 | `known` / `queue` / `readRecently` / `lastDisclosure` 折叠、`selfKnown`、删除感知、Bash `cwd` + `extractBashTargetDirs`、提醒文案逐字一致。差异：目录从计划阶段推导（失败/取消的调用也会贡献目录，v2 仅 `executed`）；指令文件变化用每轮 metadata 检查代替宿主 watch；telemetry 未发 |
| `bashTargets.ts` | `tools/bash_target_dirs.rs` | 等价 | 列表命令、flag 参数、`find` 前置路径、`cd` 基准、`pathe` 归一化与失败形状逐项移植（含语料测试） |

### progressTrack（2 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `progressTracker.ts` | `turn_loop/progress_track.rs` | 等价 | 只保留决定 `blindMutations` 的状态（`mutatedBases` / `debt` / `blind`）；v2 其余计数器仅供未发出的 telemetry |
| `progressTrackerService.ts` | `turn_loop/progress_track.rs` + `toolset::observe_progress` + `run_turn.rs` | 部分→已补 | 本批把 tracker 从 per-turn 移到 toolset（会话级，跨轮债务保留），每轮只重置注入标志；EBM 文案与阈值一致；telemetry 未发 |

## 第二批改动文件

| 文件 | 改动 |
| --- | --- |
| `src/tools/tool_select_state.rs` | 重写为 v2 `dynamicTools.ts` 语义：历史折叠/剥离/`<tools_relevant>` 渲染/最新用户查询；移除与参考实现不符的 revision ledger 与 pending-update 提醒 |
| `src/tools/toolset.rs` | `take_disclosure_announcement(history)` 实现 v2 `loadableToolsAnnouncement`（大目录检索、历史去重、空目录一次性提示）；`supports_dynamic_tool_loading` 纳入 `tool_use`；新增 `observe_progress`；删除 ledger 字段 |
| `src/tools/mod.rs` | 删除 `announced_tools` / `tool_select_state` / `announcement_revision` / `empty_disclosure_announced`；新增会话级 `progress_tracker` |
| `src/tools/select_tools.rs` | unknown 文案对齐 v2（引导 query 搜索）；移除加载后的 `<tools_added>` delivery（schema 走逐步工具表） |
| `src/tools/dispatch.rs` | `select_tools` 两个分支统一按 `enabled()`（flag + `dynamically_loaded_tools` + `tool_use`）拒绝；loadable 集合去条件化 |
| `src/tools/tests.rs` | 测试适配新签名/能力门；新增大目录 `<tools_relevant>` 切片与去重用例 |
| `src/turn_loop/run_turn.rs` | `shape_history` 替换 `shapeUnsupportedDynamicToolContext`；披露 provider 传历史并改用 `loadable-tools` 变体；删除 `tool_update_pending`；进度打分走会话级 tracker |
| `src/turn_loop/progress_track.rs` | 文档改为会话级 |
| `src/turn_loop/run_turn/tests/tool_select_history.rs` | 重写：不可加载模型剥离公告；仅保留最新 `<tools_relevant>` |
| `src/callbacks/tests.rs` | 能力向量补 `tool_use` |

## 结论（第二批）

- `toolSelect` 与参考实现的两处结构性差异已闭合：公告不再依赖引擎内 revision ledger，而是折叠会话历史（因此恢复会话不会重播旧公告）；可加载目录 ≥20 时按最新用户请求 BM25 取前 10 输出 `<tools_relevant>`，不再把数百个 MCP 工具一次性列入上下文。
- 与参考实现不符的 `notifyUpdatedTools`（"tool has an update but not applied"）提醒已移除：当前 v2 HEAD 无该行为。
- `progressTrack` 的盲改计数改为会话级，跨轮债务不再丢失。

---

## 第三批：`agent/usage` / `tokenCounting` / `state` / `goal/judge` / `task` / `media` / `contextMemory` / `contextProjector`

### usage（9 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `usage.ts` | `session/usage.rs`（`UsageStatus` / `record`） | 等价 | `byModel` / `total` / `currentTurn` 与 `UsageRecordedContext` 的消费面一致 |
| `usageEvents.ts` | `server/engine.rs`（`agent.status.updated`）、`server/activity.rs`（`phase`） | 等价 | 六种 phase、usage/model/context/plan/spec/swarm/permission 字段均有映射 |
| `usageOps.ts` | `session/usage.rs` 持久化 | 等价（通道不同） | `UsageRecord` 是 v2 的 durable agent 事件；引擎直接把计数持久化到 session usage，状态等价、事件日志不单列 |
| `errors.ts` | 无落点 | NA | `usage.turn_id_conflict` 在 v2 也只有声明、无抛出点 |
| `cacheBreak.ts` | `session/cache_observation.rs` | 等价 | cause 枚举、快照、阈值字段一致 |
| `cacheBreakService.ts` | `server/engine.rs` 的 per-session `CacheBreakTracker` | 等价 | 2000 token / 0.95 阈值、compaction 重置、`cache_break_detected` 事件 |
| `cacheObservation.ts` | `session/cache_observation.rs` | 等价 | `build_cache_observation` 字段一致 |
| `cacheProbe.ts` | 无落点 | NA | 空接口 |
| `cacheProbeService.ts` | `subagent/manager.rs` + `compaction/mod.rs` | 等价 | fork 首轮与 compaction 两条 `prompt_cache_probe` 来源均已实现 |

### tokenCounting（5 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `configSection.ts` | `config/mod.rs` `[tokenCounting]` + `KIMI_TOKEN_COUNTING_STRATEGY` | 等价 | 三种策略、大小写/空白解析、非法值告警 |
| `tokenCounting.ts` | `session/token_counting.rs` | 等价 | `ContextSize` / 请求形状语义一致 |
| `tokenCountingOps.ts` | `token_counting.rs` 的 `measured` / `truncate` / `rebase` / `record_turn` | 等价（通道不同） | v2 用 durable 事件重放；引擎直接持久化 anchors |
| `tailAnchor.ts` | `injection/goal_plan.rs` | 缺失→已补 | 本批补 `should_reanchor_tail` / `render_tail_contract` / `plan_tail_contract` + `goal_tail` provider（20k 阈值、1200 字符上限、goalId 变化重置） |
| `tokenCounter.ts` | `compaction::estimate_message_tokens`（启发式） | 部分 | wasm/sidecar/http 计数层在引擎无后端；v2 默认链同样回退到启发式 |

### state（2 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `agentState.ts` | 无落点 | NA | DI 状态注册表接口 |
| `agentStateService.ts` | `callbacks` 的 state_read/write + `storage/state_store.rs` | NA | 引擎无进程内 state registry；durable 域由宿主或本地 store 承载 |

### goal/judge（3 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `judgePrompt.ts` | `goal/judge.rs` | 等价 | 系统提示、用户提示、`parseVerdict`（直读/围栏/内嵌对象）逐字移植 |
| `goalJudgeService.ts` | `goal/judge.rs` + `run_turn.rs` | 部分→已补 | 本批补 transcript 判定：`UpdateGoal complete` 前发起独立请求，拒绝不写状态、impossible 改写为 blocked、网络/解析失败 fail-open。差异：无 per-request `maxOutputSize` 上限；blocked 的 reason 无法经 state bridge 持久化；v2 的 subagent 判定路径本身未接线（NA） |
| `judgeAgentProfile.ts` | 无落点 | NA | 只服务于 v2 未接线的 subagent 判定路径 |

### task（12 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `configSection.ts` | `config/mod.rs` `[task]` + `[background]` 合并 | 等价 | 字段、env、legacy 覆盖顺序一致 |
| `printDefaults.ts` | `session/mod.rs` `PRINT_WAIT_CEILING_S_DEFAULT` 等 | 等价 | 默认值见 `session/mod.rs` / `napi_bindings/params.rs` |
| `errors.ts` | `primitives/errors/domains.rs` | 等价 | `task.task_id_empty` / `task.limit_exceeded`（retryable） |
| `types.ts` | `storage/task_runner.rs`（`TaskStatus` / wire） | 等价 | 状态词表、终态集合一致 |
| `task.ts` | `storage/task_runner.rs` API + `StateStore::read_task_output_tail` | 等价（2026-11-10 补齐） | list/getOutput/stop/stopSession/wait/reconcile/limits 对应；`readOutputTail` 已返回 `{offset,data}`，持久化 miss 才走对齐后的内存 preview，I/O 错误透传 |
| `taskOps.ts` | `task_runner` 的 `task` 域持久化 + `event.task.*` | 等价 | start/terminated 折叠与通知触发一致 |
| `taskService.ts` | `task_runner.rs` + `session/wake.rs` + `tools/task_tools.rs` | 部分 | 生命周期、settled retention、通知去重、stop 宽限、wait、print settle、输出 tail 已对齐；detach 的宿主回调语义仍待独立复核，不提前判整个模块完成 |
| `notificationXml.ts` | `TaskNotification::to_xml` | 缺失→已补 | 本批按 v2 输出 `<notification>`：id/category/type/source_kind/source_id/agent_id、Title/Severity、wall time + 状态句、subagent 恢复指引、`<output-file>` / `<output-preview>` |
| `persist.ts` | `task_runner.rs` + `StateStore::{task_output_path,write_task_output,read_task_output,read_task_output_tail}` | 等价（2026-11-10 复核补齐） | ranged snapshot/raw tail、4-byte 回看对齐、默认 200/自定义 settled retention、ghost 与持久化输出保留已闭合；host-driven 自定义配置的跨层传递另列 |
| `textAlign.ts` | `storage/state_store.rs::align_to_character_start` | 等价 | offset≤0 由调用点 saturating 归零；continuation byte 循环与 v2 逐字节相同，供 snapshot 与内存 tail fallback 共用 |
| `wallTime.ts` | `turn_loop/wall_time.rs` | 等价 | `X.XXX seconds` 格式一致 |

### media（16 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `configSection.ts` | `config/mod.rs` `[image]` | 等价 | `read_byte_budget` / `max_edge_px` |
| `file-type.ts` | `native/file_type.rs` | 等价 | 扩展名/MIME 嗅探 |
| `image-compress.ts` | `native/image_compress.rs` | 等价 | 压缩、降采样、格式策略入口 |
| `image-format-policy.ts` | `llm/image_policy.rs` | 等价 | 接受 MIME 集合与转换规则 |
| `image-originals.ts` | `native/image_compress.rs`（原图保留） | 部分 | 引擎直接持有原图字节，无独立的 originals 注册表 |
| `imageConfigBridge.ts` | `tools/toolset.rs::with_image_limits` | 等价 | 配置 → 工具集 |
| `kimiFileUrl.ts` / `mediaRef.ts` | `llm/media_resolver.rs`（`KIMI_FILE_SCHEME` / `parse_daemon_file_url`） | 等价 | URL 解析与引用重写 |
| `mediaReadContext.ts` | `tools/read_media.rs` + `llm/prompt_media.rs` | 等价 | 读取预算与提示词注入 |
| `mediaResolver.ts` / `mediaResolverService.ts` | `llm/media_resolver.rs`（含内联/上传分支） | 等价 | 解析、上传或内联的选择 |
| `promptMediaIntake.ts` | `llm/prompt_media.rs` | 等价 | 用户媒体入口 |
| `sessionMediaStore.ts` / `sessionMediaStoreService.ts` | `server/files.rs`（附件 store） | 等价（宿主面） | 会话媒体落盘与引用解析 |
| `videoUpload.ts` | `llm/files_upload.rs` | 等价 | 视频上传/内联 |
| `webp-animated.ts` | `native/image_compress.rs::is_animated_webp` | 等价 | 动图检测 |

### contextMemory（17 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `types.ts` | `turn_loop/types.rs::LLMMessage` | 部分 | 引擎消息只建模 role/content/blocks/tool_calls/tool_call_id/prompt_id/origin；providerMessageId、timing、toolCallDisplays 等宿主元数据不进入引擎 |
| `contextEvents.ts` / `contextMemory.ts` / `contextMemoryService.ts` / `contextOps.ts` | `native/event_store/*`、`session/sqlite_store.rs` | 等价 | 追加/折叠/清空/compaction 替换均落地；事件溯源由 event store 承担 |
| `contextTranscript.ts` | `native/event_store/context_transcript.rs` | 等价（2026-11-10 新增） | reducer/reduce 双入口、完整 JSON ContextMessage、entries/times 平行输出、undo/clear/compaction foldedLength；证据见「第四批：contextTranscript」 |
| `loopEventFold.ts` | `native/event_store/loop_fold.rs` | 等价 | 空内容丢弃、tool 顺序、悬空 tool 自愈一致 |
| `compactionHandoff.ts` / `compactionText.ts` / `compaction-summary-prefix.md` | `compaction/mod.rs` | 等价 | 前缀、elision、continuation、头尾 token 截断一致 |
| `conversationTime.ts` | `session/mod.rs::origin_is_undo_anchor` + `storage/state_store.rs` | 等价 | undo anchor 判定逐条一致 |
| `conversationUndoParticipants.ts` | `storage/state_store.rs` 的 turn 折叠 | 等价 | 撤销锚点与 checkpoint 一一对应 |
| `hookParts.ts` | `llm/wire.rs` + hook 注入路径 | 等价 | `source: user prompt submit hook` 过滤一致 |
| `messageId.ts` | 无落点 | NA | 引擎消息无 id；宿主/投影层生成 |
| `openToolExchange.ts` | `compaction/mod.rs` + `subagent/fork.rs` | 等价 | 悬空 tool 以 `INHERITED_IN_FLIGHT_TOOL_OUTPUT` 补齐 |
| `toolResultRender.ts` | `turn_loop/tool_result_render.rs` + `turn_loop/wall_time.rs` | 等价 | ERROR/empty 状态、wall time、note 追加顺序一致 |
| `vacuousContent.ts` | `llm/wire.rs::project_vacuous_messages` | 等价 | 空白文本/空 think 丢弃 |

### contextProjector（5 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `projection.ts` | `llm/wire.rs::project_strict` / `project_vacuous_messages` | 等价 | 配对/重排/孤儿丢弃/重复 call-result/连续 assistant 合并/空消息丢弃 |
| `contextProjector.ts` / `contextProjectorService.ts` | `turn_loop/turn_step.rs` 的投影选择 | 等价 | strict 与 lenient 的选择条件一致 |
| `mediaProjection.ts` | `llm/media_budget.rs` | 等价 | degrade/strip、保留最近 2 个媒体、预算驱逐 |
| `requestSize.ts` | `llm/media_budget.rs` 的请求字节估算 | 等价 | 文本/think/媒体 URL/tool 参数逐项计入 |

## 第三批改动文件

| 文件 | 改动 |
| --- | --- |
| `src/goal/judge.rs` | 新增：v2 judge 提示词、verdict 解析、transcript 判定（fail-open）、`goal_snapshot_from_domain` |
| `src/injection/goal_plan.rs` | 新增 `goal_tail` provider（tail contract 重锚）与纯函数/测试 |
| `src/turn_loop/run_turn.rs` | `UpdateGoal complete` 前置 judge；拒绝合成为结果、impossible 改写 blocked；run_count 兼容合成调用 |
| `src/turn_loop/run_turn/tests/goal_judge.rs` | 新增：拒绝不写状态、impossible 改写 blocked |
| `src/storage/task_runner.rs` | `TaskNotification` 扩展 kind/agent_id/stop_reason/output_path/size/truncated；`to_xml` 对齐 v2 `renderNotificationXml` + 正文/恢复指引/输出块 |
| `src/session/mod.rs` | 任务通知改用 `to_xml` |
| `src/session/tests.rs` | 通知渲染断言同步 |

## 结论（第三批）

- 新增 goal 完成判定：模型调用 `UpdateGoal complete` 时先由独立请求对照转录核验，拒绝时目标状态不变，`impossible` 时改写为 blocked；判定失败按 v2 规则放行，不会阻塞已完成的目标。
- 新增 goal 尾部 contract：活跃目标每增长 20k token 重新在上下文尾部锚定 objective 与完成条件。
- 后台任务完成通知改为 v2 的 `<notification>` XML（含 subagent 恢复指引与输出文件/预览块），此前是简化的一句话。
- 其余 usage / tokenCounting / state / task / media / contextMemory / contextProjector 的既有实现经核对与参考实现一致或属宿主职责（详见上表）。

---

## 第四批：`agent/profile` / `fullCompaction` / `microCompaction` / `knowledge` / `mcp` / `interaction`（44 文件）

### profile（5 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `profile.ts` | `prompt/profiles.rs`、会话 `agent_config` | 部分 | `AgentProfile`/catalog/合并投影齐全；model/thinking/tools 会话状态由 `agent_config` 承载。`ProfileError` 错误域未建模（引擎以字符串/包裹错误上抛） |
| `errors.ts` | 无独立落点 | NA | 错误码由 REST/WS envelope 与 i18n 承担，引擎无 profile 错误域 |
| `profileOps.ts` | `agent_config` state 行 | NA | v2 用 durable 事件回放 profile/activeTools 状态；本 fork 直接持久化 `agent_config`，无可观察回放面 |
| `context.ts` | `prompt/agents_md.rs`、`prompt/environment.rs`、`prompt/builder.rs`、`environment/agents_md.rs` | 等价 | AGENTS.md 级联/去重/32KB 告警/`<!-- From -->`/cwd 两层树/追加目录说明一致；`splitAgentsMdFiles` 仅服务 changeNotifier 基线，Rust 由 `AgentsMdReminder` 另行承担 |
| `profileService.ts` | `server/engine.rs::session_spec`、`prompt/builder.rs`、`tools/tool_policy.rs` | 部分 | 模型/思考/禁用工具/权限/计划模式、prompt 重建、活动工具过滤、插件 64KB 预算均已对齐；profile bind 由宿主完成。AGENTS.md 超限告警与插件预算告警本批补齐；`tool-pattern-no-match` 告警仍未发（见「已识别未补」） |

### fullCompaction（13 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `compaction-instruction.md` | `compaction/compaction-instruction.md` | 等价 | 逐字节一致 |
| `compactionInstruction.ts` | `compaction/mod.rs::render_compaction_instruction` | 等价 | 自定义指令块替换与 trimEnd 一致 |
| `compactionUtils.ts` | `compaction/mod.rs` | 等价 | 常量/收缩比例/取尾/丢弃前导 tool 结果一致；本批补 `snipLargeToolResults` 与 truncated/filtered 分支 |
| `compactionGeneration.ts` | `compaction/mod.rs::summarize_with_llm_budgeted` | 等价 | 溢出收缩、空摘要丢弃、重试 ladder、usage/cache probe 一致；本批修截断恢复与丢弃未生效的问题 |
| `compactionApply.ts` | `compaction/mod.rs::{post_process_summary, in_flight_turn_messages}`、`run_turn` 落地 | 部分 | TODO 追加、in-flight turn 收尾一致；wire 行区间、关联记忆写注、非历史状态指纹属 v2 专属结构（见 NA 行） |
| `compactionOps.ts` | `turn_events.rs` / `run_turn.rs` / `server/engine.rs` | 部分 | started/cancelled/completed 及 manual 路径已发；`compaction.blocked` 不适用（Rust 同步压缩，没有可等待的 in-flight 任务） |
| `contextRecovery.ts` / `context-recovery-footer.md` | 无落点 | NA | 依赖文件型 wire journal 的行区间与 `journalPath`；本 fork 用 SQLite 行日志，模型侧无行号概念 |
| `errors.ts` | `compaction/mod.rs::CompactionError` | 等价 | failed/unable 两个错误码承载；本批新增 `Truncated` |
| `fullCompaction.ts` | 模块函数接口 | 等价 | 服务接口由 `compaction` 模块的纯函数 + 编排承载 |
| `fullCompactionService.ts` | `turn_loop/run_turn.rs`、`server/engine.rs` | 等价 | 手动/自动入口、溢出恢复、轮内上限、最后压缩 token、节流重置均已实现 |
| `strategy.ts` | `compaction/mod.rs::CompactionConfig` / `should_compact` / `compute_compact_count` / `can_split_after` | 等价 | 触发/阻断/保留窗口/分裂点语义一致 |
| `types.ts` | `CompactionReport` / `CompactionReportEvent` | 等价 | result 字段一一对应 |

### microCompaction（4 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `flag.ts` | `[experimental].micro_compaction`（`server/engine.rs::micro_compaction_config`） | 等价 | 仅 standalone server 解析；napi 路径由宿主解析 flag |
| `microCompaction.ts` | `compaction/micro.rs::MicroCompactionConfig` | 等价 | 五个默认值逐项一致 |
| `microCompactionOps.ts` | `server/engine.rs::micro_compaction_state`（metadata） | 等价 | cutoff 持久化/clamp/reset 一致 |
| `microCompactionService.ts` | `compaction/micro.rs` + `server/engine.rs` | 等价 | detect/apply/advance/clamp/reset 与事件已接线；本批补 `micro_compaction_finished` 遥测 |

### knowledge（7 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `knowledge.ts` | `knowledge/mod.rs` 模型 + `tools/knowledge_tool.rs` | 等价 | 条目/搜索结果/统计形状一致 |
| `knowledgeInjection.ts` | `injection/knowledge.rs::register_knowledge_injection` | 等价 | 信号提取、5 条/800 token 上限、置信度 0.5 一致 |
| `knowledgeLearner.ts` | `injection/knowledge.rs::learn_from_turn` | 等价 | 纠错正则、60 字标题、0.4 置信度一致；本批修类别覆盖顺序 |
| `knowledgeService.ts` | `knowledge/mod.rs::{ensure_open, open_existing, resolve_db_path}` | 等价 | `KIMI_KNOWLEDGE_DB` → 项目库 → 用户库；读路径不建库 |
| `sqliteKnowledge.ts` | `knowledge/mod.rs` | 等价 | 表/FTS/触发器/评分一致；本批补 scope 路径归一化 + 目录边界与 FTS 逐词转义 |
| `tools/knowledge-tool.ts` | `tools/knowledge_tool.rs` | 部分 | search/add/confirm/reject 文案与语义一致；fork 额外暴露 remove/stats/import |
| `tools/knowledge-tool.md` | `tools/knowledge_tool.rs` 内嵌描述 | 等价 | 描述与使用时机文案一致 |

### mcp（9 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `mcp.ts` | `mcp/manager.rs` | 等价 | 初始加载等待本批补为 2s 预算 + 一次性过期闩；list/resolved/reconnect/status 订阅齐全 |
| `mcpDiscoveryOps.ts` | 无落点 | NA | durable 发现去重状态供 v2 内部遥测；协议不暴露，无客户端可观察面 |
| `mcpEvents.ts` | `mcp/status.rs`、`napi_bindings/mcp.rs`、`manager.rs` | 等价 | `mcp.server.status`、`tool.list.updated`、名称冲突 `error` 均已发 |
| `mcpFailureOps.ts` | 运行时 status + `session_warnings` | NA | v2 的 open/retryAt durable 状态是引擎内部回放数据；Rust 直接维护运行时状态并经 warnings 路由呈现 |
| `mcpService.ts` | `mcp/manager.rs` | 部分 | 连接状态机、同名工具去重、跨服冲突丢弃、失败重试、401→needs-auth、工具注册/注销一致；needs-auth 的合成 `mcp__<server>__authenticate` 工具未注册（见「已识别未补」） |
| `output.ts` | `mcp/output.rs` + `manager.rs::structured_extras` | 部分 | 文本/resource/data-url/10MB cap/附件保留/通知溢出均一致；本批补 structuredContent 去重、保留 `_meta` 命名空间剥离与紧凑转义。媒体-only 的 `<mcp_tool_result>` 包装因本 fork 分离 content/delivery 无法同构 |
| `tools/auth.ts` | `server/route_tools_mcp.rs` + `skills/builtin/mcp-config.md` | 部分 | OAuth 登录经 REST `/mcp-config` 流程与技能指引完成；模型侧合成认证工具同上 |
| `tools/mcp.ts` | `mcp/manager.rs::call_tool` | 等价 | 移除检查、401 处理、重连后重试、应用级错误透传一致 |

### interaction（6 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `approval.ts` | `server/interaction.rs`、`permission/mod.rs` | 等价 | 审批请求/决策/scope/feedback 与协议类型一致 |
| `question.ts` | `server/interaction.rs`、`tools/ask_user_question.rs` | 等价 | 问题项/多选/other/answer method 一致 |
| `interactionOps.ts` | `server/interaction.rs`、`events/types.rs` | 部分 | 生命周期由 `event.approval.*` / `event.question.*` 协议事件承载，无 `interaction.request/resolved` durable 事件 |
| `interactionWiring.ts` | `server/interaction.rs::{cancel_turn, cancel_session}` | 等价 | turn_ended / agent_closed 取消未决交互 |
| `engineMessages.ts` | `goal/mod.rs` 暂停原因常量、`swarm/agent_run_batch.rs` 取消文案 | 等价 | 文案逐字一致；纯判定函数在 v2 内无消费者，Rust 无需移植 |
| `reviewDecisions.ts` | `tools/exit_plan_mode.rs`、`tools/spec_mode.rs` | 等价 | approve/reject/reject_and_exit/revise 与 legacy label 映射一致 |

## 第四批改动文件

| 文件 | 改动 |
| --- | --- |
| `src/compaction/mod.rs` | 新增 tool 结果 snip（40/40 行、1KB、marker）；truncated finish reason 恢复；filtered 空响应直接失败；修空摘要丢弃只改局部变量、请求未收缩的问题 |
| `src/compaction/tests.rs` | mock 支持 empty/truncated/filtered finish；新增 5 条恢复与 snip 用例 |
| `src/turn_loop/run_turn.rs` | `provider_finish_reason` 提升为 `pub(crate)` 供压缩复用 |
| `src/knowledge/mod.rs` | scope 路径归一化 + 路径边界前缀；FTS 查询始终逐词转义；新增归一化用例 |
| `src/injection/knowledge.rs` | 纠错类别改为 v2 的依次覆盖顺序 |
| `src/mcp/manager.rs` | 初始加载 2s 预算 + 过期闩与遥测日志；`structured_extras` / `is_reserved_meta_key` |
| `src/mcp/manager/tests.rs` | structured extras 去重/保留键/转义与保留键判定用例 |
| `src/prompt/builder.rs` | 新增 `build_with_warnings`（AGENTS.md 告警）与用例；`build` 保持兼容 |
| `src/server/engine.rs` | 会话 prompt 重建时发布 `agents-md-oversized` 告警（按 session+code+message 去重）；补 `micro_compaction_finished` 遥测 |
| `src/server/plugins.rs` | `plugin_system_prompt_block_with_skipped` |
| `src/server/builder.rs` | 组装期/插件变更时发布 `plugin-sections-oversized` 告警 |
| `src/locales/{en,zh}.json`、`packages/i18n-catalog/src/locales/{en,zh}.ts` | 新增 `engine.compaction.truncated` |

## 结论（第四批）

- 压缩：截断摘要不再被当成完整摘要接受；空摘要恢复真正收缩下一次请求；大工具结果在摘要请求前裁头尾，避免压缩请求自身溢出。
- 知识库：Windows 反斜杠 scope 现在能与 `/` 形式路径互相命中，`G:/repo` 不再误匹配 `G:/repo2`；带引号的查询不再整条丢掉 FTS 命中；纠错入库类别与 v2 的覆盖顺序一致。
- MCP：初始连接最多阻塞 2 秒且只等一次，挂起的服务器不再拖死每个步骤；`structuredContent` 不再重复注入，保留 `_meta` 命名空间被剥离，`<` 已转义。
- 告警：AGENTS.md 超限与插件 prompt 预算耗尽现在作为 v2 同名 warning 事件到达客户端。

## 第四批已识别、未补（附原因）

| # | 缺口 | 原因 |
| --- | --- | --- |
| 1 | `tool-pattern-no-match` 告警（profile/global `[tools]` 永不匹配的模式） | v2 在每次 profile bind 时用完整注册表判定；本 fork 无 agent bind 时点，工具表按会话/轮动态装配（含 MCP、host 注入），启动期判定会产生误报。需先确定「告警应挂在会话首轮还是引擎启动」的产品口径 |
| 2 | needs-auth 的 `mcp__<server>__authenticate` 合成工具 | 工具内要发起授权码流程，需要宿主提供的回调基址（REST 路由从请求 Host 推导）；属宿主协作项，未擅自改造 |
| 3 | `compaction.blocked` / `interaction.request` 等 durable 事件 | 本 fork 压缩为同步执行、交互走协议 `event.approval/question.*`，无对应可观察时点 |




---

## 第五批：`agent/permission*`、`modeMutex`、注入类、checkpoint/undo、plugin、其余基础设施（83 文件）

### permission 链（35 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `permissionRules/configSection.ts` | `config/mod.rs::PermissionConfig` | 等价 | `rules` + legacy `deny`/`allow`/`ask` 折叠、`{tool, match}` 简写、`KIMI_CODE_DANGEROUS_COMMAND_GUARD` 只接受 true/false、默认开启 |
| `permissionRules/matchesRule.ts` | `permission/mod.rs::parse_permission_pattern` | 等价 | 工具名大小写、`Tool(arg)` 模式、别名映射一致 |
| `permissionRules/permissionRules.ts` | `permission/mod.rs` 规则匹配/审批记录 | 等价 | session 级 approve 规则、scope 优先级、LEGACY_TOOL_NAME_ALIASES 一致 |
| `permissionRules/permissionRulesOps.ts` | `permission/mod.rs` 状态写入 | 等价 | 状态形状与可撤销写入一致 |
| `permissionRules/permissionRulesService.ts` | `permission/mod.rs::evaluate` | 等价 | 规则来源合并顺序一致 |
| `permissionGate/permissionGate.ts` | `callbacks::execute_tool` + `permission::evaluate` | 等价 | 执行前权限门与拒绝通道一致 |
| `permissionGate/permissionGateService.ts` | 同上 | 等价 | 拒绝/取消文案、worker 指引一致 |
| `permissionPolicy/permissionPolicy.ts` | `permission/mod.rs::evaluate_in_mode` | 等价 | 15 条策略顺序与 `policy_name` 同名 |
| `permissionPolicy/permissionPolicyService.ts` | `permission/mod.rs` | 等价 | 模式解析、yolo/auto 分支一致 |
| `permissionPolicy/types.ts` | `permission/mod.rs` | 等价 | PermissionMode / PolicyDecision 同形 |
| `permissionPolicy/policies/*.ts`（15 个具名策略） | `permission/mod.rs` 同名分支 | 等价 | guardian 复核在 callbacks 中对 `YoloModeApprove` 前置；`environment-lease` NA（沙箱代替） |
| `guardian/guardianService.ts` | `permission/guardian.rs` | 等价 | 60s 超时为本 fork 已文档化差异 |
| `permissionMode/permissionMode.ts` / `permissionModeOps.ts` / `configSection.ts` / `permissionModeService.ts` | `permission/mod.rs` + `napi_bindings/session.rs::session_set_permission_mode` + 宿主 | 等价 | 模式持久化/状态事件由宿主发布（`session-set-permission` 测试），引擎负责即时生效与子代理广播；`defaultPermissionMode` 归宿主 |
| `permissionMode/injection/permissionModeInjection.ts` | `injection/permission_mode.rs` | 等价 | enter/exit 文案与 Auto 例外（ExitPlanMode 仍等审阅）已按 v2 原文；env 开关默认开 |

### modeMutex / prompt / 注入类（14 文件）

| v2 文件 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `modeMutex/modeMutex.ts` / `modeMutexService.ts` | `tools/mode_mutex.rs` + `tools/spec_mode.rs` + `tools/dispatch.rs` | 部分→已补 | spec 进入挤掉 plan/swarm/tower、tower 进入挤掉 plan/swarm/spec 已有；本批补 tower 进入暂停 active goal、goal 恢复时暂停 tower 开放 mission（等价 tower.exit('mode-mutex')）。plan 进入由宿主写状态、hello-world 模式本 fork 不存在 → NA |
| `injection/renderedLines.ts` | 无独立落点 | NA | 唯一消费者是 `features/gitMemory` / `associativeMemory`，本 fork 无这两个注入特性 |
| `interruptionReminder/*.ts`（3） | `injection/interruption_reminder.rs` | 等价 | 仅 user_cancelled 触发、lastComparableMessage 去重、每轮一次；ownerPromptId 无对应（injection 不带 prompt 归属） |
| `changeNotifier/*.ts`（3） | `injection/change_notifier.rs` + `server/engine.rs` | 部分→已补 | skills/subagents/workspace_dirs 与 agents_md_change 均实现；本批把 AGENTS.md「新增」段文案对齐 v2 原文。mute/unmute、seedBeliefs 由「首轮快照 + prompt 内容门控」等价覆盖 |
| `prompt/errors.ts` | `primitives/errors/domains.rs` | 等价 | 6 个错误码同名 |
| `prompt/messageContent.ts` | `rpc/types` ContentBlock、`turn_loop/types` LLMMessage | NA | 纯类型定义 |
| `prompt/promptEvents.ts` | `session/mod.rs`、`server/prompt_queue.rs`、`protocol/events` | 等价 | completed/aborted/submitted/steered/queued/started 均已存在（queued/started 为第一批跨层补齐） |
| `prompt/promptMetadataText.ts` | `session/title.rs` | 等价 | 200/4000 截断与多段脱敏一致 |
| `checkpoint/checkpointService.ts` | `session/sqlite_store.rs`（`session_file_history`）+ `server/route_sessions.rs` | 等价 | 写前后快照、manual_edit/deleted 冲突、`checkpoint.restored` 载荷一致；存储为文本 file-history（32MB 上限由 FILE_HISTORY 预算替代，已文档化） |
| `undo/undo.ts` / `undoService.ts` | `session/sqlite_store.rs::{plan_undo_turns, undo_turns}` + `server/route_sessions.rs` | 等价 | undo 锚点、压缩边界拒绝、`context.undone`、lastPrompt 重算一致；排队/静默期由 HTTP 路由串行化 |
| `replayBuilder/types.ts` / `fold.ts` | `native/event_store/loop_fold.rs` + `session/history` | 等价 | wire 折叠、pending tool 补齐（`Tool result is not available in the current context...`）、goal/plan/config/permission 重放面一致 |
| `blob/*.ts`（3） | `native/event_store/blobs.rs` + `native/event_store/mod.rs` + `session/sqlite_store.rs` | 等价（2026-11-10 复核补齐） | 旧记录漏判 50MiB LRU、malformed ref、空 MIME、按 ref MIME 展开、DB 读取失败与直接媒体字段边界；逐项证据见「第二批：blob 持久化底座」 |
| `testScopeGuard/*.ts` | `turn_loop/test_scope_guard.rs` | 等价 | 命令正则、narrowing flag、repo root 判定、env 开关、每轮一次均逐字一致 |
| `llmRequester/*.ts`（4） | `llm/**`、`turn_loop/turn_step.rs`、`config/mod.rs`、`compaction` | 等价 | 32MB byte budget + env、413 降级/剥离、`prompt_cache_probe`/cache break、credential/turn 快照均已有 |
| `userTool/*.ts`（3） | `callbacks::list_tools` / `execute_tool` | 部分 | 宿主注入的工具可列出、可执行；durable `tools.register_user_tool` 事件与子代理继承由宿主目录承担（本 fork 无 DI/agent state）→ NA |
| `plugin/*.ts`（3） | 新增 `injection/plugin.rs` + `server/engine.rs` + `server/plugins.rs` + `skills::PluginSkillDir` | 部分→已补 | 本批实现 `plugin_session_start`（首轮注入、含 `<plugin_session_start>`/`<plugin-instructions>`、冻结与 supersede、无插件时的 neutralized 通知、main agent）与 `plugin_change`（install/enable/disable/remove 文案，按会话水位每会话一次）；`refreshSessionStart` 的强制刷新参数已留出（`force_refresh`），宿主 /reload 尚无接线 → 记为宿主项 |
| `pluginCommand/*.ts`（2） | `server/plugins.rs` 命令解析 + 宿主 `plugin_command.activated` | NA | 激活流程（展开 `$ARGUMENTS`、发事件、更新 lastPrompt）由 node-sdk 完成 |
| `command/*.ts`（3） | 无 | NA | DI 命令贡献点；本 fork 无 DI 容器 |
| `actorService/agentActorService.ts` | 无 | NA | xstate actor + DI 基础设施 |
| `agentContext/agentContext.ts` / `agentSpace.ts` | 无 | NA | DI 作用域/模型空间 |
| `scopeContext/scopeContext.ts` | `RunTurnInput.agent_id`、`subagent` 管理器 | NA | 宿主/子代理 id 由显式参数承载 |
| `shellCommand/*.ts`（2） | 宿主 `runShellCommand`（node-sdk） | NA | 引擎只负责 Bash 工具；交互式 `!` 命令在宿主 |
| `environmentBinding/*.ts`（5） | `environment/**`、`subagent/manager.rs`、`server/engine.rs` | 部分 | 本地环境声明/冻结/恢复/子代理绑定已实现；`environment.set_binding` 与 `environment.status.changed` 无落点（本 fork 拒绝远程环境、无运行时切换）→ NA |

## 第五批改动文件

| 文件 | 改动 |
| --- | --- |
| `src/tools/mode_mutex.rs` | 新增 `pause_goal_for_tower_enter`（TowerModeEnter 暂停 active goal）与 `evict_tower_for_goal_activation`（goal 恢复时暂停 tower 开放 mission），补 3 条测试 |
| `src/tools/dispatch.rs` | towerinit 成功后暂停 active goal；UpdateGoal/CreateGoal 成功后按目标状态挤掉 tower |
| `src/server/engine.rs` | AGENTS.md「新增」段文案对齐 v2；新增 `PluginChangeNotices` 队列（每会话水位）、`publish_plugin_change_notice`、在 `observe_catalog_changes` 注入 `plugin_change` |
| `src/server/builder.rs` | `publish_plugin_changed(kind, id)` 同时入队模型可见提醒 |
| `src/server/route_plugins_oauth.rs` | 安装/启用/停用/移除传 mutation kind |
| `src/injection/plugin.rs` | 新增 plugin session-start/change 注入模块与 9 条测试 |
| `src/injection/mod.rs` | 注册 `plugin` 模块 |
| `src/skills/mod.rs` | `PluginSkillDir` 增 `session_start`（manifest.sessionStart.skill） |
| `src/server/plugins.rs` | `plugin_skill_dirs` 填 `session_start` |
| `src/tools/toolset.rs` | 新增 `plugin_skill_dirs()` / `session_id()` 读取器 |
| `src/turn_loop/run_turn.rs` | main agent 首轮注册 plugin session-start 注入 |
| `src/tools/skill.rs` | `expand_skill_parameters` / `strip_frontmatter` 提升为 `pub(crate)` |
| `src/native/mod.rs` | `escape` 模块 `pub(crate)` |
| `src/injection/permission_mode.rs` | AUTO_ENTER_REMINDER 回到 v2 原文（ExitPlanMode 仍等待用户审阅计划） |

## 结论（第五批）

- 权限链 35 文件逐项等价：配置兼容写法、15 条策略顺序、guardian 前置复核、模式注入文案均与 v2 一致。
- 模式互斥补齐两处状态交互：tower 进入暂停 active goal；goal 恢复时暂停 tower 开放 mission，避免 goal 续跑与 tower 工蜂写集分叉。
- 插件：会话启动技能现在会以 `plugin_session_start` 注入（含插件 instructions、会话 id 展开、冻结/替换语义）；插件安装/启停/移除会在下一次模型请求中携带 `plugin_change` 提醒。
- changeNotifier 的 AGENTS.md「新增」段与 v2 原文一致。

## 验证（第五批）

- `cargo check --lib` 通过（仅剩其他窗口 `subagent_timeout_ms` dead_code 告警）。
- `cargo test --lib injection::plugin` 9 通过；`tools::mode_mutex` 15 通过；`injection::permission_mode` 8 通过；`injection::change_notifier` 16 通过；`server::plugins` 30 通过；`skills::` 24 通过。
- 未跑全量回归（用户规则：提交前才全量）。

### 第五批补充：`agent/tools/`（48 文件）

| v2 分组 | Rust 落点 | 判定 | 说明 |
| --- | --- | --- | --- |
| `os/bash/{bash,bashTool,process-task}.ts` | `tools/bash_exec.rs`、`tools/shell_results.rs`、`tools/stream_sink.rs`、`native/bash.rs`、`storage/task_runner.rs` | 等价 | 60/300s、后台 600/86400s、`timeout must be ≤ Ns`、空命令/缺 description、超时转后台、`Command executed successfully.` / `Command failed with exit code: N.`、信号终止、250ms drain、16MiB/1MiB 截断一致 |
| `os/powershell/*.ts` | `tools/powershell.rs`、`native/powershell.rs` | 等价 | `-EncodedCommand`、`-NoProfile -NonInteractive`、UTF-8、`KIMI_PWSH_PATH`、超时/后台常量与文案一致 |
| `os/glob/*.ts` | `native/glob.rs`、`tools/file_ops.rs`、`tools/mod.rs` | 等价 | 默认 100、offset、`include_ignored`、`include_dirs` 弃用忽略、Windows 路径提示、字符上限与续读一致 |
| `os/grep/*.ts` | `native/grep.rs`、`tools/search.rs`、`tools/grep_types.rs` | 等价 | 默认 250、offset、三种 output_mode、`-n` 默认 true、`-C` 优先、multiline、VCS 排除、敏感文件过滤、20s 预算、512KB 上限一致 |
| `os/read/{read,readTool,configSection}.ts` | `native/read.rs`、`tools/file_ops.rs`、`tools/read_media.rs`、`config/mod.rs` | 等价 | 100k/500k、10MB transcode、负 line_offset、column_offset、region/full_resolution、`[read]` 蛇形/驼峰与非法值告警一致 |
| `os/write/*.ts`、`os/edit/*.ts` | `native/write.rs`、`native/edit.rs`、`tools/file_ops.rs`、`tools/core_tool_defs.rs` | 等价 | overwrite/append、父目录创建、old/new string 视图、replace_all、CRLF 还原、`old_string not found` / `No changes to make...` 一致 |
| `os/shell/shellToolResults.ts` | `tools/shell_results.rs`、`tools/bash_exec.rs`、`tools/powershell.rs` | 等价 | timeout label、60 字命令预览、后台/前台 next_step、detached 文案一致 |
| `select-tools/*.ts` | `tools/select_tools.rs`、`tools/tool_select_state.rs`、`tools/tool_retriever.rs` | 等价 | names/query 二选一、strict、`Provide names to load or a query to search.`、加载/搜索分流一致 |
| `read-media-file/{read-media-file,execute-media-read}.ts` | `tools/read_media.rs`、`llm/media_resolver.rs`、`llm/media_budget.rs`、`native/image_compress.rs` | 等价 | 100MB、256KB/5MB inline budget、2000px、64MB 解码、region/full_resolution、HEIC 分支、`<image path=...>` 一致 |
| `web-search/*.ts` | `tools/web_search.rs`、`native/web_search.rs` | 等价 | query schema、结果形状一致 |
| `fetch-url/*.ts` | `tools/fetch_url.rs`、`native/fetch_url.rs` | 等价 | 10MB、10 跳、HTML 抽取、moonshot 优先、cite 提示一致 |
| `agent/{agent,agentTool,subagent-task}.ts` | `tools/agent_tool/**`、`subagent/manager.rs`、`storage/task_runner.rs` | 等价 | 2h 默认超时、fork/resume/model pool、后台 task_id `agent-*`、partial output 两段文案、repeat breaker settle reason 一致 |
| `ask-user-question/*.ts` | `tools/ask_user_question.rs`、`server/interaction.rs` | 部分 | 1-4 题/2-4 选项/唯一性/多选/dismissed/unsupported/background 可用性门控一致；`background=true` 的后台任务注册由宿主完成（`AskQuestionRequest.background` 文档约定），引擎不重复建任务；重复项错误文案为通用式而非 v2 逐项文案 |
| `task/task-list|task-output|task-stop|task-wait/*.ts`、`task/tools/format.ts` | `tools/task_tools.rs`、`storage/task_runner.rs` | 等价 | active_only/limit、输出/截断行、stop 文案、WaitFor 1-90s、repeat warning 三套文案、wall time 格式一致 |
| `github/*.ts` | `tools/github.rs`、`tools/github/{specs,schema,execute,http}.rs` | 等价 | draft-07 schema、token/base URL 解析链、pretty JSON 校验错误、credentials/`GITHUB_TOKEN`/`GH_TOKEN` 一致 |
| `fileMemory.ts` | 无 | NA | 依赖 `features/gitMemory`，本 fork 无该特性 |
| `fileReadSource.ts` | `tools/core_tool_defs.rs` Read 描述、`llm/media_resolver.rs` | 等价 | `kimi-file://` 附件、server-local path 提示、分页保持引用一致 |
| `mainAgentOnly.ts` | `goal_tools.rs`、`memory_action.rs`、`dispatch.rs` | 等价 | Cron/Goal/Memory/SessionQuery/DefineSubagent 的主代理限定一致 |

## 第二批已识别、移交 `tool` 线（`src/tools/**` 自本批起由 tool 线独占）

| # | 缺口 | 位置 | 证据 |
| --- | --- | --- | --- |
| 1 | `KIMI_CODE_EXPERIMENTAL_WAIT_FOR` / `[experimental].wait_for` 开关未实现，WaitFor 恒在工具表 | `tools/tool_policy.rs::native_tool_defs_with_notify` | v2 `task/task-wait/flag.ts`（默认 true，可显式关闭） |
| 2 | MCP 工具所属服务器被移除后的调用文案缺失（v2: `MCP server for tool "X" has been removed (plugin uninstalled or config deleted). Do not call this tool again.`） | `mcp/manager.rs::call_tool` 返回 `None` → `tools/dispatch.rs` 未知工具路径 | v2 `agent/mcp/tools/mcp.ts:42-49` |
| 3 | AskUserQuestion 重复问题/重复选项使用通用错误文案，v2 逐项指出重复值 | `tools/ask_user_question.rs::parse_request` | v2 `ask-user-question.ts:52-70` |

## 第五批补充结论（agent 目录收尾）

- `agent/` 下 51 个子目录已完成逐文件落点映射；`tools/` 48 文件按 18 组核对，schema/描述/限额/错误文案均有对应，剩余 3 条移交 `tool` 线。
- `mcp.server_failed` / `mcp.server_failure_resolved` / `mcp.tools_discovered` 三条 durable 事件无同名记录：本 fork 以 `mcp.server.status` + `tool.list.updated` 承载同样的可观察状态，失败/重试信息在 `McpServerEntry.nextRetryAt` 上，属事件模型差异（已记录，不视为可观察缺口）。

## 第五批补充（收尾）

- `PluginChangeNotices` 增加「会话首轮水位」语义：会话在第一轮之前排队的插件变更不会回放（该会话的 prompt 已按当前插件集渲染），只有已经跑过至少一轮的活跃会话才会收到下一次变更通知。新增 3 条单测：`server::engine::plugin_change_notices_tests`。
- `agent/plugin` 的会话启动技能目前只在本轮带 `NativeToolset` 时注入（run_turn 的注册点读取 `toolset.plugin_skill_dirs()`）；host-proxy（无原生工具）轮次没有插件目录来源。补齐需要在 `RunTurnInput` 增加插件目录字段（跨构造点），待 `tool` 线释放 `run_turn.rs` 后处理，已记录为待办。
- 验证追加：`cargo test --lib plugin_change_notices_tests` 3 通过；格式化后 `cargo check --lib` 通过。

## 第五批补充（napi/TUI 路径闭环）

- `plugin_change` 队列提升为进程级（`injection/plugin.rs::PluginChangeNotices`，`publish_plugin_change_notice` / `take_plugin_change_notices`）：HTTP 路由与 napi 插件安装/启停/移除/MCP server 开关现在写入同一队列，`ServerEngine` 与会话泵共用每会话水位。
- napi 会话泵（`session/mod.rs::pump` → `inject_plugin_reminders`）在每轮 `history + prompt` 组装前注入两类引擎自有提醒：
  - `plugin_change`：该会话未读的插件变更（首轮只建水位、不回放旧变更）；
  - `plugin_session_start`：复用 `session_start_reminder` 的冻结/supersede 语义，来源为会话 toolset 的插件技能根。
- 这些提醒在 HTTP 引擎仍走注入注册表；napi 路径没有 `ServerEngine`，改为直接进入交给 LLM 的历史，`run_turn` 的注册表看到历史中的同名注入后保持静默，不会重复。
- 验证：`injection::plugin` 12 通过（新增 3 条队列用例）；`cargo check --lib` 通过。

### 仍缺（napi/TUI 的 changeNotifier 其余变体）

`skills_change` / `subagents_change` / `workspace_dirs_change` / `agents_md_change` 目前只在 `ServerEngine` 的每轮观察里产生，napi 会话泵没有对应实现。补齐需要：`NativeToolset` 暴露 `skill_dirs` / `extra_roots` / `subagent_manager`（`src/tools/**` 自本批起由 tool 线独占，取数接口未暴露）、`AgentsMdWatch` 的渲染函数从 `server/engine.rs` 提升到共享模块，以及宿主 prompt 的「已包含清单」门控输入。已记录为 tool 线释放 `src/tools/**` 后的待办。

### 基线红项（非本批）

`cargo test --lib session::` 中 3 条 print-steer/task-notification 用例（`test_print_steer_ignores_another_sessions_notifications` 等）在**关闭本次注入调用后仍失败**，属另一窗口在途修改（`session/mod.rs`、`storage/task_runner.rs` 均处于未提交修改状态），不计入本批。

---

## 2026-11-10 严口径复核与完整补齐队列

### 复核元信息与判定口径

- 参考：`D:\kimi\kimi-code` @ `4f6ac94f525f03b266657dba252d268440a6bbd2`，以工作区磁盘现状为准。
- 参考侧 `agent/` 有未提交变化：删除 `goal/judge/judgeAgentProfile.ts`、`tokenCounting/tokenCounter.ts`；修改 `toolSelect/{dynamicTools,toolSelect,toolSelectAnnouncementsService,toolSelectService}.ts`、`tools/edit/edit.md`、`tools/os/read/read.md`；新增 `toolSelect/toolSelectOps.ts`。
- 当前实测为 **51 个一级目录、288 个 `.ts` 文件**；含 24 个 Markdown 资产共 **312 个文件、45,399 行**。将 `goal/judge` 独立计为功能模块时，对应此前「52 个子模块」口径。
- 旧表保留为模块到 Rust 落点的索引与历史证据；其中「等价」「NA」不直接继承为终态。每批仍按入参与选项、分支覆盖、错误路径、默认值与常量、副作用与顺序、输出结构与字段、并发与重入七维复核。
- 旧表完全漏掉 `contextMemory/contextTranscript.ts` 和当前新增 `toolSelect/toolSelectOps.ts`。另有 112 个文件仅以通配或分组出现，实施时必须展开到具体文件与行为点，不能用分组名称代替七维证据。

### 当前目录树实测

| 一级目录 | 文件数（含资产） | 行数 | 一级目录 | 文件数（含资产） | 行数 |
|---|---:|---:|---|---:|---:|
| `actorService` | 1 | 160 | `agentContext` | 2 | 184 |
| `agentsMdReminder` | 3 | 604 | `blob` | 3 | 212 |
| `changeNotifier` | 3 | 812 | `checkpoint` | 1 | 238 |
| `command` | 3 | 96 | `contextMemory` | 17 | 2,168 |
| `contextProjector` | 5 | 838 | `environmentBinding` | 5 | 492 |
| `fullCompaction` | 13 | 1,895 | `goal`（全部位于 `judge/`） | 2 | 363 |
| `guardian` | 1 | 251 | `injection` | 1 | 43 |
| `interaction` | 6 | 269 | `interruptionReminder` | 3 | 116 |
| `knowledge` | 7 | 1,024 | `llmRequester` | 4 | 1,447 |
| `loop` | 13 | 4,667 | `mcp` | 8 | 1,536 |
| `media` | 16 | 2,827 | `microCompaction` | 4 | 330 |
| `modeMutex` | 2 | 211 | `permissionGate` | 2 | 110 |
| `permissionMode` | 7 | 211 | `permissionPolicy` | 21 | 1,077 |
| `permissionRules` | 5 | 416 | `plugin` | 3 | 308 |
| `pluginCommand` | 2 | 141 | `profile` | 5 | 1,938 |
| `progressTrack` | 2 | 336 | `prompt` | 4 | 280 |
| `replayBuilder` | 2 | 903 | `scopeContext` | 1 | 55 |
| `shellCommand` | 2 | 296 | `state` | 2 | 93 |
| `task` | 12 | 2,896 | `testScopeGuard` | 2 | 205 |
| `tokenCounting` | 4 | 188 | `toolActivation` | 3 | 147 |
| `toolApproval` | 2 | 317 | `toolDedupe` | 2 | 614 |
| `toolExecutor` | 6 | 1,426 | `toolPolicy` | 4 | 272 |
| `toolRegistry` | 4 | 284 | `toolResultTruncation` | 2 | 369 |
| `toolSelect` | 10 | 1,497 | `tools` | 66 | 9,156 |
| `undo` | 2 | 301 | `usage` | 9 | 513 |
| `userTool` | 3 | 267 |  |  |  |

### 已确认缺口分布

| 层 | 缺口 | 当前判定与处置 |
|---|---|---|
| 当前参考漂移 | `toolSelectOps` 的 durable initialized/tools_changed 状态；registry reconcile 串行化；snapshot/delta revision 与 disclosure；runtime origin；schema 签名失效；`tool_update_pending`；active history 中动态 schema 的保留/剥离 | 旧实现缺少整段新状态机，且旧表误把 `tool_update_pending` 当作 v2 已删除行为。优先在引擎内补齐，不改协议。 |
| 完全漏项 | `contextTranscript` 的 wire reduction、undo/clear/compaction foldedLength、entries/times 平行输出 | 先核对 `native/event_store` 与 `session/sqlite_store` 的真实调用者；引擎内可闭合则实现。 |
| 纯叶与文案 | `toolRetriever` 缺 NFKC；WaitFor flag 未接；MCP 已移除服务器文案未接；AskUserQuestion 重复项文案不精确；AGENTS.md 目录采集包含未执行调用；Knowledge 额外 `remove/stats/import`；`edit.md`/`read.md` 当前资产需复核 | 作为第一批纯叶修正；额外工具面按「不自创」复核并移除，不顺手改其他行为。 |
| 持久化与任务 | `readOutputTail`/`textAlign`；`maxSettledTasks` 淘汰；TaskStop reason/终态抑制与 WaitFor 去重；`toolSelectOps` 状态恢复；`contextTranscript` | 先补存储与折叠底座，再接工具和服务；声称「跨层」的项必须先证明引擎内确实无数据或写点。 |
| 媒体与 blob | originals 注册表；50MiB byte LRU；spill 路径；消息元数据与恢复计数字段 | 不得再把缺实现直接写成「仅性能差异」；逐文件确认可观察面和真实消费者。 |
| 服务与注入 | goal judge 输出上限/blocked reason；profile 无匹配 pattern 警告；MCP authenticate 与媒体-only 输出；plugin host-proxy 来源；napi changeNotifier 其余变体；环境绑定状态事件；interaction durable 事件 | 按依赖从数据提供者到 run_turn/session 接线逐组闭合；需要新增协议字段或事件的立即列入跨层清单并跳过。 |
| 需重新取证的旧 NA | `actorService`、`agentContext`、`command`、`state`、`toolActivation`、`toolContribution`、`builtinToolAssemblyService`、`scopeContext`、`environmentBinding` 等 | 旧理由主要是「DI/架构不同/无独立落点」，不符合本次口径。只有证明纯类型、死代码、或现有 Rust 路径七维等价后才可保留 NA。 |

### 实施顺序

1. **叶工具与资产**：NFKC、WaitFor flag、MCP/AskUserQuestion 文案、AGENTS.md 执行成功口径、知识工具面、read/edit 资产。
2. **状态与持久化底座**：`toolSelectOps`、`contextTranscript`、task tail/retention、blob originals/LRU；同步所有真实调用者。
3. **工具与媒体服务**：toolSelect 新 reconcile/disclosure 状态机、MCP、profile、goal、task 服务。
4. **上下文与注入**：contextMemory/contextProjector/fullCompaction、plugin/changeNotifier/environmentBinding/interruption。
5. **loop 与 agent 容器面**：loop/actorService/agentContext/state/command/toolActivation，并复核全部旧 NA。
6. 每批补齐后立即跑指定门禁并回填本表；基线 13 条失败单独对照，不改测试或断言换绿。

### 跨层清单（本轮跳过）

- `userTool`：v2 需要 durable `tools.register_user_tool`/`tools.unregister_user_tool`、宿主执行回调、子代理继承与冲突规则；这些事件和回调字段当前不属于 `packages/kimi-agent` 单侧契约，保持登记，不新增平行协议。
- B1 剩余事件字段/事件类型（step `messageId`/`cache`/`cacheBreak`、`turn.step.retrying`、统一 interaction durable 事件、agent 级取消原因）：若复核确认必须改 `packages/protocol` 或 `packages/node-sdk`，只记录具体字段与消费点。
- B3 `ContentBlock.hidden` 与图片宽高、B5 `PolicySnapshot.scope`、B6 宿主后台字段，以及必须由 `apps/kimi-code` 编排的 `/init`、shellCommand/pluginCommand：不动契约包，不伪造宿主数据。

### 当前局限

- 本次是旧对账表的覆盖率与缺口复核，不把旧表的静态「等价」当作七维终审；每个实施批次仍需读取对应 v2 文件全文并补充逐项证据。
- Rust 主工作树存在 `_base` 迁移、注释清理和其他会话在途改动；全部保留。门禁红项按实际改动归属记录，不回滚、不代修测试清单会话负责的失败。

### 第一批：纯叶行为实施记录（2026-11-10）

| 行为点 | v2 依据 | Rust 落点与七维结果 | 判定 |
|---|---|---|---|
| 工具检索 NFKC | `agent/toolSelect/toolRetriever.ts:112-136` | `tools/tool_retriever.rs::tokenize` 在切词前以 `ComposingNormalizer::new_nfkc()` 归一，再小写化、Unicode alphanumeric 切词和 CJK bigram；空查询、stopwords、文档频率与排序路径不变。新增全宽 `ＣＲＥＡＴＥ ｐｕｌｌ ｒｅｑｕｅｓｔ` 命中 `create_pr` 用例。 | 等价 |
| AskUserQuestion 重复值错误 | `agent/tools/ask-user-question/ask-user-question.ts:49-69`；`askUserQuestionTool.ts:101-112` | `tools/ask_user_question.rs::question_uniqueness_error` 在后台能力门之后、宿主提问之前，按首次重复的问题文本/选项标签返回逐字错误；其他 malformed 输入仍走原通用错误，宿主调用顺序不变。 | 等价 |
| WaitFor 默认开启与禁用 | `agent/tools/task/task-wait/flag.ts:3-16`；`taskWaitTool.ts:155-164,423-428` | `tools/task_tools.rs::wait_for_enabled` 以 `KIMI_CODE_EXPERIMENTAL_WAIT_FOR` → `[experimental].wait_for` → `true` 求值；`native_tool_defs_with_notify` 关闭时不披露，`dispatch` 对直接调用返回 v2 原文。 | 等价（见原生替代限制） |
| MCP 已移除服务器 | `agent/mcp/tools/mcp.ts:22-49` | `mcp/manager.rs` 保留 qualified-name → server 的移除墓碑，`mark_removed` 写入、重新注册清除、shutdown 清空；`dispatch` 将墓碑路由到 `call_tool`，返回 v2 逐字错误而非通用 unknown-tool。断连/普通失败不写墓碑，`remove_server` 不伪造 `mark_removed`。 | 等价 |
| AGENTS.md 仅采集已执行调用 | `agent/agentsMdReminder/agentsMdReminderService.ts:107-115,186-218,239-305` | `turn_loop/run_turn.rs` 在调度结果按原始调用索引对齐后，只为实际执行且取得执行结果的调用采集目录；preflight/synthetic/skipped/cancelled/execute Err 不采集，dedupe 复用结果不重复采集。原生工具返回 `isError=true` 但确已执行仍采集；AGENTS.md 自读抑制同样只计成功且已执行的 Read/Edit/Write。 | 等价 |
| Knowledge 工具面 | `agent/knowledge/tools/knowledge-tool.ts:17-26,45-115` | `tools/knowledge_tool.rs` 的 schema、执行分派与测试只保留 `search/add/confirm/reject`；删除 v2 没有的 `remove/stats/import` 及 `markdown` 参数，既有四 action 的输入、输出和错误路径不变。 | 等价 |
| Read/Edit 描述资产 | `agent/tools/os/read/read.md:1-16`；`agent/tools/edit/edit.md:1-5` | `tools/core_tool_defs.rs` 删除自述为引擎新增的 Read/Edit 段落；保留文本与 v2 在常量插值后的内容一致。WaitFor 描述同步删除 v2 文件末尾不存在的自述句（`task-wait.md:1-17`）。 | 等价 |

#### 原生替代登记

| v2 机制 | Rust 原生能力 | 可观察等价理由 | 覆盖不到的场景 |
|---|---|---|---|
| `String.prototype.normalize('NFKC')`（`toolRetriever.ts:125-136`） | `icu_normalizer::ComposingNormalizer::new_nfkc` | 同一 Unicode NFKC 形式，后续小写化与 token 序列相同。 | 未发现；排序中的 JS `localeCompare` 与 Rust 字节序差异属于既有排序口径，不归此次归一化替代，toolSelect 批次另行复核。 |
| `IFlagService.enabled('wait_for')`（`flag.ts`、`taskWaitTool.ts`） | 进程环境读取 + `KimiConfig::discover` + `ExperimentalValue` | env 优先、配置其次、默认 true；禁用披露与直接调用拒绝一致。 | 宿主只在内存中覆盖 FlagService、既不写 env 也不写配置的场景引擎不可见；需宿主契约传入，未擅自增加字段。 |
| `isRemoved` 闭包（`agent/mcp/tools/mcp.ts:22-49`） | `HashMap<String,String>` 墓碑 + 服务器状态机 | 同一 qualified name 在移除后得到固定错误，重连/重新注册恢复真实调用，普通断连不会误报 removed。 | 进程重启后内存墓碑消失，与 v2 进程内工具注册表的生命周期一致；无额外持久化。 |
| `ToolDidExecuteContext.outcome === 'executed'`（`agentsMdReminderService.ts:186-188`） | 调度器索引对齐 + 实际执行时长 `Option` + dedupe 原始索引 | preflight/合成/跳过/取消/执行 Err 都没有实际执行结果；正常返回即使 `isError=true` 仍有执行记录，目录与自读口径一致。 | 无；当前调度器对所有真实 `Ok` 执行结果记录时长，对所有未执行/执行 Err 结果不记录。 |
| `JSON.stringify` 重复值插值（`ask-user-question.ts:58,64`） | `serde_json::to_string` | 对可进入引擎的有效 UTF-8 文本，引号、反斜杠、控制字符与非 ASCII 输出一致。 | JS 字符串可含孤立 UTF-16 surrogate，Rust UTF-8 `str` 无该值域；该边界不能伪造对齐。 |

#### 验证命令与退出码

- `cargo check --lib`：初次退出 101（本批索引引用错误），修正后退出 0。
- 定向测试：`tool_retriever` 8、`ask_user_question` 13、`knowledge_tool` 11、WaitFor flag 1、MCP mark_removed 1、bash operand 8、executed-error 1、tower veto 1；8 条命令退出码均为 0。
- `cargo check --features cli`：退出 0。
- `cargo test --lib`：退出 0，**4378 passed / 0 failed / 0 ignored**。本批删除 4 个非 v2 Knowledge action 测试并新增 4 个对齐测试，净测试数变化为 0；与上一会话 4381 的总数差为并行工作树既有变化，未新增失败。
- `bun run scan:hardcoded:rust`：初次退出 1（6 新增/16 陈旧）；在 `scripts/.hardcoded-allowlist.lock` 内逐行增删并验证 3352 唯一条目后复跑退出 0，new=0、stale=0。
- `bun run check:rust-module-deps`：退出 0，336/336。
- `bun run check:parity`：退出 0（REST 125、tools 85、napi 106）。
- 触碰文件 `rustfmt --check --edition 2024 --config skip_children=true`：本批行已修正；复跑仍退出 1，唯一 diff 是 `turn_loop/run_turn.rs:2352` 的既有 `and_then` 格式，不在本批 diff，未代修。
- `bun run check:architecture`：退出 1，`kimi-agent` stored fingerprint=`2bca66ebe3396f93`、current=`aa226a566f73aa8b`；共享 `architecture.json` 已在其他会话中修改，本批不执行 `--update`、不整写指纹，留待统一收口。既有 i18n-runtime warning 不变。

#### 剩余项

- 本批只关闭表内行为点，不把整个 `toolSelect`、`mcp`、`task` 或 `agentsMdReminder` 模块提前判完成。
- WaitFor 的宿主纯内存 FlagService 覆盖仍是跨层输入缺口；未新增 NAPI/protocol 字段。
- 下一层处理 `toolSelectOps`、`contextTranscript`、task tail/retention 与 blob originals/LRU。

### 第二批：blob 持久化底座（2026-11-10）

| v2 文件 | 七维取证 | Rust 落点 | 判定 |
|---|---|---|---|
| `agent/blob/agentBlobService.ts:4-13` | 常量逐字为 `blobref:` 与 `[media missing]`；接口只有 offload/load/isBlobRef，无额外事件或返回字段 | `native/event_store/blobs.rs` 的同名常量、URL 前缀判断、`offload_data_uris` / `rehydrate_blob_refs`；无新增对外面 | 等价 |
| `agent/blob/agentBlobServiceImpl.ts:8-127,131-155` | 入参是 ContentPart 列表；只处理 part 直接值中含 `url` 的非数组对象，不深入媒体对象。`data:` 头要求非空 MIME，payload 按 UTF-16 code unit 数达到 4096 才 offload；已有 blobref 不变。hash 是 payload UTF-8 的 SHA-256；先 put 成功再写缓存。load 对所有 `blobref:` 前缀生效，parse 失败、hash 为空、缺字节或 store.get 拒绝均为 `[media missing]`；空 MIME 合法；缓存只按 hash 存 bytes，每个 ref 用自己的 MIME 重建 data URI | `blobs.rs::{transform_media_urls,parse_data_uri,parse_blob_ref,decode_lenient_base64,read_blob}`；`SqliteEventStore` 与 `SqliteSessionStore` 各持 `BlobCache`，并贯通 append/fold/state/checkpoint/export/get_wire_events/repair 的真实调用点。事件 append 仍先 clone/parse，state 仍只在含 data URI 时 clone，事务与事件写入顺序未变 | 等价 |
| `agent/blob/byteLruCache.ts:1-42` | 默认 50×1024×1024 bytes；get 命中刷新 recency；set 先删除旧同键；单值超上限不缓存且保留其他项；容量不足逐个逐出最旧项；替换时重新计 bytes | `blobs.rs::ByteLruCache`，由 `BlobCache` 内 `parking_lot::Mutex` 保护；8 条 v2 分支均有同名 Rust 单测对应 | 等价 |

#### 原生替代登记（blob）

| v2 机制（文件：行） | Rust 原生能力 | 可观察等价理由 | 覆盖不到的场景 |
|---|---|---|---|
| 插入有序 `Map<string, Buffer>`（`byteLruCache.ts:2-40`） | `HashMap` + `VecDeque` + `Arc<Vec<u8>>`；逐 store `parking_lot::Mutex` | 命中、刷新、替换、超上限、多项逐出和当前字节数顺序一致；Arc 只避免复制，缓存字节在服务内从不原地修改，因此无 JS Buffer 别名差异；Mutex 使并发 get/set 的 recency 与容量核算原子化 | 没有 agent 服务内的可观察缺口；不把这个私有缓存暴露为可变 Buffer API |
| `Buffer.from(payload, 'base64')`（`agentBlobServiceImpl.ts:124`） | 按 UTF-16 code unit 低 8 位读取，过滤无效字符、转换 base64url、遇 `=` 停止，再由 `base64::GeneralPurpose` 以 indifferent padding + allow trailing bits 解码 | Bun 实测缺 padding、ASCII 空白、`-_`、尾随 `!`、padding 后内容、非 BMP 输入；Rust 单测逐项断言相同字节，包括 `😀YQ → empty`、`𝄞YQ → e184`。hash 仍对原 payload 的 UTF-8 计算，与 v2 一致 | JS 字符串可含孤立 UTF-16 surrogate，Rust 有效 UTF-8 `str` 无该值域；JSON 能进入 Rust 的字符串均覆盖 |
| `Bun.CryptoHasher('sha256').update(payload, 'utf8')`（`agentBlobServiceImpl.ts:123`） | `sha2::Sha256` + 十六进制格式化 | 输入字节、摘要长度和小写 hex 相同；同 payload 跨 MIME 仍同 hash，而 blobref 各自保留 MIME | 无 |
| `IBlobStore` + agent scope（`agentBlobServiceImpl.ts:15-22,97-106,122-127`） | 既有 SQLite `wire_blobs` 表 + 每个 `SqliteEventStore` / `SqliteSessionStore` 独立 50MiB 缓存 | put 后才写缓存；get 先查缓存，miss/无行/数据库错误都映射 missing；同 hash 内容相同所以不以完整 data URI 为缓存值；持久化格式未新增协议字段 | 物理存储不是 v2 的 scope 目录，故不覆盖绕过 store 直接删改物理文件/表后的跨作用域比较；agent 模块正常 offload/load 面无删除 API，读写结果等价 |

#### 验证命令与退出码（blob）

- `cargo check --lib`：退出 0（仅既有 unused warnings）。
- `cargo test --lib native::event_store::blobs`：首次退出 101，原因是新测试 `uri` 移动后使用；修正测试所有权后退出 0，**16 passed / 0 failed**，其余 4375 条 filtered。
- `cargo test --lib`：退出 0，**4391 passed / 0 failed / 0 ignored**；相比上一批 4378 净增 13 条（原 3 条 blob 测试扩为 16 条），无新增失败。
- `cargo check --features cli`：退出 0。
- `rustfmt --edition 2024 --check src/native/event_store/blobs.rs src/native/event_store/mod.rs src/session/sqlite_store.rs`：初次退出 1，仅本批 3 处排版；手工修正后复跑退出 0。
- `git diff --check -- <上述三文件>`：退出 0。
- `bun run scan:hardcoded:rust`：退出 0，recorded=found=3352、new=0、stale=0；本批未改 allowlist。
- `bun run check:rust-module-deps`：退出 0，336/336。
- `bun run check:parity`：退出 0（REST 125、tools 85、napi 106）。
- `bun run check:architecture`：退出 1，stored fingerprint=`2bca66ebe3396f93`、current=`a2bb16e8bab223dc`；共享指纹同时包含其他会话在途改动，未执行 update、未代修。

#### 剩余项（blob 之后）

- `blob/` 三文件本批闭合；`media/` 的 originals 注册表、spill 路径、消息元数据与恢复计数字段不属于 `blob/`，仍在媒体批次处理。
- 下一批按底座顺序处理 task `textAlign` / `readOutputTail` / `maxSettledTasks`，随后 `contextMemory/contextTranscript` 与 `toolSelectOps`。

### 第三批：task 输出与 settled retention（2026-11-10）

| 行为点 | v2 依据 | Rust 落点与七维结果 | 判定 |
|---|---|---|---|
| UTF-8 起始边界 | `agent/task/textAlign.ts:1-8`；`persist.ts:20,241-274` | `storage/state_store.rs::align_to_character_start` 使用相同 continuation-byte 位掩码；snapshot 先取目标起点前最多 4 bytes 的窗口，再在窗口内对齐，`previewBytes` 仍是请求预算而非实际解码字节数。`a + 3×😀`、预算 5 的预览恰为 1 个 emoji | 等价 |
| persisted raw output tail | `agent/task/persist.ts:217-239,313-334`；`taskService.ts:664-669` | `StateStore::read_task_output_tail` 以 metadata 得 size，`offset=max(0,size-maxBytes)`，range read `[offset,size-1]`，不做字符对齐；`maxBytes=0` 直接返回空 data 和 size offset。`NotFound` 才返回 `Ok(None)`，其他 metadata/open/seek/read 错误以 `io::Result` 透传。`TaskRunner::read_output_tail` 仅在 persisted miss 时走内存 preview，fallback offset=0 且使用字符对齐；未知 id 返回空 data | 等价（Rust 入参为非负整数 `usize`，见替代登记） |
| settled task retention | `taskService.ts:159-163,559-579,771-779,923-933,1152-1170,1172-1213` | `TaskRunner` 默认 `max_settled=200`，配置最小正值覆盖；每次首次 settle 记录 FIFO，超出后把最旧终态从 live map 转为无 output/outputPath 的 ghost。`entry/list/wait/get_output` 可查 ghost，持久化 output/raw tail 仍可读，`stop` 只查 live map 因而对 ghost 返回 not-found；running entry 永不逐出。重复 settle 提前返回，终态事件/通知/持久化完成后才逐出；tasks→ghosts 锁顺序保证读写不穿过中间态 | 等价（host-driven 自定义配置传递见跨层项） |
| 配置解析与合并 | `agent/task/configSection.ts:19-38,73-90` | 既有 `[task]` 覆盖 `[background]`、无 maxSettledTasks env binding 的口径不变；`KimiConfig::background_limits` 新增内部 `max_settled_tasks`，pipeline/server/repl 将 `resolve_max_settled_tasks()` 接入 runner，缺省保持 200 | 等价（限引擎已有配置通道） |

#### 原生替代登记（task）

| v2 机制（文件：行） | Rust 原生能力 | 可观察等价理由 | 覆盖不到的场景 |
|---|---|---|---|
| `Uint8Array` continuation-byte 扫描（`textAlign.ts:1-8`） | `&[u8]` + `usize` 索引 | 位运算、前进方向、到 buffer 末尾停止完全一致；所有调用点的 offset 都由文件大小或字符串长度计算，不会出现负值 | 独立 helper 不接受 JS 的负/小数 offset；它不是对外 API，v2 真实调用点也只产生非负整数 |
| `readStream({start,end})`（`persist.ts:230-236,313-334`） | `File::metadata` + `Seek` + `Read::take` | 起止 offset、读取字节和 maxBytes=0 短路一致；snapshot 只读取 preview+4 窗口，raw tail 只读取请求尾部，不把整份日志装入内存 | Rust 接口以 `usize` 接收调用方已经规范化的非负整数；没有新增 JS Number 的 NaN/负数/小数解析面，当前 crate 也没有该外部输入通道 |
| live `Map` + durable `ghosts`（`taskService.ts:236-238,336-338,1152-1170`） | `Mutex<HashMap<String,TaskEntry>>` + `Mutex<HashMap<String,Value>>` + `VecDeque` + `AtomicUsize` | live 详细条目数量受 200/配置值约束，ghost 仍提供任务信息；output 字符串只留在持久化日志和未淘汰条目中。同一线程顺序由锁与 settle 尾部调用保证 | v2 ghost 由 state 事件回放恢复；Rust 当前只在新 settle 时生成 ghost，历史 task.json 的 ghost 重建属于 restore/reconcile 后续项，不在本批伪装闭合 |
| `resolveAgentTaskConfig` 读取（`taskService.ts:1154-1156`） | 既有 `KimiConfig` → `BackgroundLimits` → `TaskRunner` | 同一配置对象在 runner 创建/应用 limits 时读取，未配置保持 200，正值即时影响下一次 settle | host-driven NAPI/stdio 入口没有该内部值，详见下列跨层项；未擅自读取第二份配置或新增协议字段 |

#### 跨层跳过（本批新增）

- **host-driven NAPI/stdio 的自定义 `maxSettledTasks` 传递**：`napi_bindings/params.rs::JsRunTurnParams` 与 `rpc/types.rs::RunTurnParams` 只携带 maxRunningTasks 等既有值，没有承载已解析 maxSettledTasks 的字段。要让这些入口使用非 200 配置，必须在 `packages/node-sdk` 的 native params 构建和 RPC 调用侧同步传递；本轮禁止改这些层，故只跳过该传递项。默认 200 与 engine 自己加载配置的 pipeline/server/repl 路径均已实现，不新增 NAPI 字段冒充 v2 行为。

#### 验证命令与退出码（task）

- `cargo test --lib storage::task_runner`：退出 0，**52 passed / 0 failed**，4344 filtered。
- `cargo test --lib task_output`：退出 0，**9 passed / 0 failed**，4387 filtered。
- `cargo test --lib`：退出 0，**4396 passed / 0 failed / 0 ignored**；相比 blob 批 4391 净增 5 条（2 条 state output、3 条 retention/fallback）。
- `cargo check --features cli`：退出 0。
- 触碰文件 `rustfmt --edition 2024 --check`：初次退出 1，仅本批排版；修正后最终退出 0。触碰文件为 `storage/{mod,state_store,task_runner}.rs`、`storage/task_runner/tests.rs`、`config/mod.rs`、`pipeline/mod.rs`、`server/builder.rs`、`repl/mod.rs`。
- `git diff --check -- <上述文件>`：退出 0。
- `bun run scan:hardcoded:rust`：退出 0，recorded=found=3352、new=0、stale=0；未改 allowlist。
- `bun run check:rust-module-deps`：退出 0，336/336。
- `bun run check:parity`：退出 0（REST 125、tools 85、napi 106）。
- `bun run check:architecture`：退出 1，stored fingerprint=`2bca66ebe3396f93`、current=`1530fc1dc9add389`；共享指纹含其他会话在途改动，未 update、未代修。

#### 剩余项（task 批之后）

- 本批不把整个 `taskService` 判完成：detach 的宿主回调、历史 ghost 的 restore/reconcile 仍按后续七维复核处理。
- 下一批处理完全漏项 `contextMemory/contextTranscript`，随后当前 v2 新增的 `toolSelect/toolSelectOps` 与 dynamic tool 状态机。

### 第四批：contextTranscript 完整 reducer（2026-11-10）

| v2 文件/行为点 | 七维取证 | Rust 落点 | 判定 |
|---|---|---|---|
| `contextTranscript.ts:14-47` 的输入与输出 | 输入为 Iterable WireRecord；输出严格为 entries、times、foldedLength，entries[i] 与 times[i] 同源，time 可为 undefined | `native/event_store/context_transcript.rs::{ContextTranscript,ContextTranscriptReducer,reduce_context_transcript,create_context_transcript_reducer}`；`Option<Value>` 保留时间数值与缺省，`result(&self)` 返回平行快照，reducer 可继续 add | 等价 |
| `contextTranscript.ts:49-97` + `loopEventFold.ts:95-227` | step.begin 先 settle 前帧；只接受当前 stepUuid；vacuous 空帧丢弃；tool.call 的 args 缺省为 null、存在时 JSON.stringify；Map pending 保序；tool.result 携带 isError/note/白名单 durationMs；step.end 写 usage/两项 timing，interrupted/error 不 settle；deferred 在 pending 清空后按序释放 | reducer 内实现完整 loop fold，消息使用 `Value` 而非现有窄 `Message`；pending 用 Vec 保序且同 id 更新名称不换位置；复用同模块中断文案，wall-time 名称与 MCP 前缀逐字一致。现有 `loop_fold.rs` 的文本专用 sink 与 clear-on-reset 未被改动 | 等价 |
| `contextTranscript.ts:99-119` + `conversationTime.ts:17-37` | undo 跳过 injection、遇 compaction_summary 停止；从尾部删到指定 anchor 数；删除每个 anchor 前连续 ownerPromptId 相同的 injection；重复 prompt id 只删最近所属 injection；clearFloor 不可跨越；结束重置 open/pending/deferred 而不清空 transcript | `apply_undo` 使用与 JS for 循环相同的可回退 cursor；origin 缺失与存在但缺 kind 分别处理，inTurn/user-slash 规则逐条一致 | 等价 |
| `contextTranscript.ts:121-160` 的 compaction/clear | keptUserMessageCount 存在先 settle，否则 reset 但保留既有帧；summary 字符串优先于 contextSummary，再支持 ContextMessage.content 文本拼接；clear 保留 entries、将 foldedLength 清零并抬高 clearFloor | `read_compaction_summary_text` 与 compaction 分支顺序一致；summary 直接推入 transcript，不先自增 foldedLength | 等价 |
| `contextTranscript.ts:189-237` 的 foldedLength 恢复 | kept count → +2/+3；legacy compactedCount<旧 foldedLength → 1+差值；否则从 clearFloor 后收集真实用户输入，按 20,000 token 从尾部选择并 +1 | `recover_folded_length` 用 `f64` 保留 v2 number 语义；fallback 只计算选择数量，复用 crate 既有字符 token 估算与 MEDIA_TOKEN_ESTIMATE=2000，覆盖超长消息截断仍计 1 条的分支 | 等价 |

#### 原生替代登记（contextTranscript）

| v2 机制（文件：行） | Rust 原生能力 | 可观察等价理由 | 覆盖不到的场景 |
|---|---|---|---|
| `ContextMessage` / `MutableMessage`（`contextTranscript.ts:25-41,172-187`） | `serde_json::Value` + `Map` | append_message 只复制 v2 选中的 id/role/content/toolCalls/toolCallId/isError/origin/usage/llmTiming；tool message 另行保留 note/durationMs；不因 Rust 窄消息类型丢字段 | JS readonly/对象身份与调用方可变别名不映射；Rust result 深拷贝快照，JSON 值与字段顺序的可观察内容一致 |
| 插入有序 pending `Map`（`loopEventFold.ts:112-129,179-210`） | `Vec<(String,String)>` + position/remove | 同 id 更新名称保持原位置，多个悬空 tool 按注册顺序补中断结果；result 只消费匹配 id | 无 |
| `String.prototype.trim`（`vacuousContent.ts:3-19`） | 显式 ECMAScript whitespace `trim_matches` | text/think 空白判定一致；特别覆盖 JS 包含 FEFF 但不包含 NEL U+0085，避免直接 Rust trim 的集合差异 | 无 |
| `estimateTokensForMessage` + WeakMap（`compactionHandoff.ts:226-245`；`human/llm/tokens.ts:108-166`） | 既有 `crate::compaction::{estimate_tokens,estimate_tokens_for_json,MEDIA_TOKEN_ESTIMATE}` | role/content/think/media/tool name/JSON arguments 使用同一字符估算；fallback 只需要选中条数，因此不生成截断消息也无需缓存，结果一致 | JS 可含孤立 surrogate 或非 JSON 值，Rust Value 不承载；schema 内 WireRecord 均覆盖 |
| `createLoopEventFold` sink（`loopEventFold.ts:78-97`） | reducer 内部状态机 + 同模块常量复用 | 避免复用 Rust 文本专用 sink 导致 ContentPart/metadata 丢失，也避免其 reset 清空 transcript；没有新增 `native -> turn_loop` 模块边 | v2 live fold 的 WeakMap/immer 状态恢复属于 `loopEventFold.ts` 后半独立能力，不冒充本 reducer 所需语义；本 reducer 从空状态构造，无该输入 |

#### 消费者与边界

- 全仓检索 v2 `src` 后，`reduceContextTranscript` / `createContextTranscriptReducer` 当前只有 `contextTranscript.ts` 自身与 `test/agent/contextMemory/contextTranscript.test.ts` 使用，没有生产服务调用点。Rust 因此提供等价 crate 内公共 reducer，不伪造 session/server 接线或新增事件。
- 与 v2 相同，WireRecord/schema 是输入前置条件；未为超出契约的 JSON 形状自创错误码、默认值或修复行为。
- 12 个 Rust 测试以分组场景覆盖 v2 测试中的普通流、3 种 compaction 计数、时间/metadata、pending/deferred、retry/vacuous、undo/clear、prompt-owned injection、legacy open frame 与超长 token fallback。

#### 验证命令与退出码（contextTranscript）

- `cargo test --lib native::event_store::context_transcript`：首次退出 101（11 passed / 1 failed），multi-turn undo 的 Rust range 未按 JS 内层 cursor 回退；按 `contextTranscript.ts:99-119` 改为可变 cursor 后复跑退出 0，**12 passed / 0 failed**。
- `cargo test --lib`：退出 0，**4408 passed / 0 failed / 0 ignored**；相比上一批 4396 净增 12 条。
- `cargo check --features cli`：退出 0。
- 新文件单独 `rustfmt --edition 2024`：退出 0；`rustfmt --check context_transcript.rs event_store/mod.rs` 复跑退出 0。新文件无 `//`、`/* */`、`///` 注释。
- `git diff --check -- context_transcript.rs event_store/mod.rs`：退出 0。
- `bun run scan:hardcoded:rust`：初次退出 1（新增中断文案）；改为复用 `loop_fold::TOOL_INTERRUPTED_ON_RESUME_OUTPUT` 后退出 0，recorded=found=3352、new=0、stale=0；未改 allowlist。
- `bun run check:rust-module-deps`：初次退出 1（`native -> turn_loop`）；移除 wall-time 跨模块调用后退出 0，336/336；未更新依赖基线。
- `bun run check:parity`：退出 0（REST 125、tools 85、napi 106）。
- `bun run check:architecture`：退出 1，stored fingerprint=`2bca66ebe3396f93`、current=`6f63af90f620ad4e`；共享指纹含其他会话在途改动，未 update、未代修。

#### 剩余项（contextTranscript 之后）

- 本批只闭合旧表完全漏掉的 `contextTranscript.ts`；`contextMemory/types.ts` 的窄 `LLMMessage` 映射和既有 `loop_fold.rs` 后半恢复能力仍按各自文件复核，不借新 Value reducer 提前改判。
- 下一批处理当前 v2 `toolSelect/toolSelectOps.ts` 及 dynamicTools/toolSelect/Announcements/Service 的 durable initialized/tools_changed、reconcile、revision/disclosure、runtime origin、schema 签名和 active history。

### 第五批：toolSelect durable 状态机与动态公告（2026-11-10）

| v2 文件/行为点 | 七维取证 | Rust 落点 | 判定 |
| --- | --- | --- | --- |
| `toolSelectOps.ts:9-35,38-48,85-105` | ToolSelectTool 含 name/description/parameters/source/disclosure/origin；initialized 仅首次生效且保持事件顺序；change 先按 removed 名称删除，再追加 added、按名称排序，并逐项追加 changes | 新增 `tools/tool_select_ops.rs` 的 `ToolSelectModelState::{initialize,change,apply_wire_event}`；事件类型严格为 `tool_select.initialized` / `tool_select.tools_changed`，payload 用 `agentId` | 等价 |
| 同上 durable/replay | 恢复按 session+agent 折叠，不能串 worker/main 或其他 session；写入失败不能更新内存或通知集合 | `restore_tool_select_state`、`persist_tool_select_{initialized,tools_changed}` 使用既有 `wire_events`；`NativeToolset::reconcile_tool_select` 成功落库后才写内存；server 通过 `set_tool_select_persist_listener` 注入 own-tail 记录，避免下轮误报 `session.stale` | 等价 |
| `toolSelectService.ts:140-151,380-463` | prepare 串行；首轮 initialized；后续只按名称增删，added 标 runtime；同名 pending update 按 version signature 去重并发送 `tool_update_pending` 原文 | `plan_tool_select_reconcile` + 每工具实例 `tokio::sync::Mutex`；`ToolSelectPendingUpdate`；run_turn 在注入前拉取工具表，并把多条提醒插到 `loadable-tools` 前 | 等价 |
| `toolSelectService.ts:465-509,608-629,705-733` | capableTools 以 initial 为基线、同名取 current、runtime 仅在 disclosure 时纳入；select_tools 使用独立 disclosure policy；runtime inline 同样需加载；关闭 disclosure 不含 runtime/select_tools | `toolset.rs::shape_tool_table_with_state` 使用 durable initial/current，不再只看名称快照；`dispatch.rs::tool_registration` 按 durable dynamic 属性拦截；空目录也保留 select_tools | 等价（限当前 ToolInfo 可表示的 builtin/MCP/inline user 工具） |
| `toolSelectService.ts:251-378` + `toolSelectAnnouncementsService.ts:21-29` | feature flag 与 model capability 分开；revision=changes.len；lastDisclosure 只取最近同 variant 的有效结构化值；先按 revision 发 delta，否则 legacy 修正/快照；initial inline 删除仍保留 removed | `tool_select_state.rs::ToolSelectAnnouncementDisclosure`；`toolset.rs::take_disclosure_announcement` 实现 snapshot/delta/legacy 三分支；run_turn 将 disclosure 写入 `origin.disclosure` | 等价 |
| `dynamicTools.ts:17-29,59-123,160-189,262-277` | retrieved 只认 `retrieved-tools` variant；loadable added/removed 可认 injection/system_trigger；关闭 disclosure 时 snapshot 删除、delta 转 runtime reminder，select_tools 调用及结果成对清理 | `tool_select_state.rs` 移除文本识别与 Rust 自创 legacy variant；实现 runtime 文案、select 调用清理及 disclosure 名称优先 | 等价（system message.tools 相关分支另列跨层） |
| `toolSelectService.ts:516-591` | 失败环境包含 policy 与完整工具定义（description/parameters/source/disclosure/origin），公告边界也重置；search/announce 均发 tool_retrieval，query_chars 为 UTF-16 code units | `select_tools.rs::selection_environment_for_tools`；公告和 load 调用点统一重置；`callbacks.telemetry` 发 announce/search 字段，matched 分支与零分候选一致 | 等价 |
| `toolRegistryService.ts:61-96,108-135` + `mcpService.ts:351-418` | 重连/替换时旧注册项 dispose，cached 与 registry 不得分别保留不同工具集 | `mcp/manager.rs::register_client` 在替换 cached 前记录旧 qualified names，成功注册新集合后注销 stale registry 项；保留同名 pending 版本 | 等价 |
| `toolSelectSchemasService.ts:20-27`、`dynamicTools.ts:17-19,99-103,125-158`、`toolSelectService.ts:238-249,539-555,631-703` | system role 的空 content 消息携带 tools；pending schema 按名称排序 drain；历史 schema 合并、删除通知、签名失效和 active history 保留/剥离 | 当前 `kimi-core::LLMMessage` 没有 tools 字段，无法表示或恢复该消息，更无法让 Kimi provider 发出 `message.tools` | 缺失（跨层跳过，见下） |
| `userTool/userTool.ts:4-9`、`userToolService.ts:97-116` | 宿主 user tool 可显式 `disclosure: deferred`，registry source 为 user；inactive/active 也影响初始注册表 | `kimi-core::ToolInfo` 与 NAPI `JsToolDef` 只传 name/description/input_schema，宿主 source/active/disclosure 无法还原 | 缺失（跨层跳过，见下） |

#### 原生替代登记（toolSelect）

| v2 机制（文件:行） | Rust 原生能力 | 可观察等价理由 | 覆盖不到的场景 |
| --- | --- | --- | --- |
| `defineState` + durable AgentEvent2（`toolSelectOps.ts:38-105`） | serde 类型 + SQLite `wire_events` + reducer 回放 | 同一事件类型、字段、首次初始化和 change 折叠顺序；按 session/agent 隔离，事件 append 成功才提交内存状态 | 无 `SqliteSessionStore` 的独立 run_turn 只能保进程内状态，不能跨进程恢复；server/有 store 路径已覆盖并登记 own-tail |
| `reconcileTail: Promise<void>`（`toolSelectService.ts:74,140-151`） | 每工具实例 `tokio::sync::Mutex<()>` | 同一实例的 prepare 串行，失败不提交；后一次可重新从 durable 状态计算，不会交错覆盖 revision | 不同 NativeToolset 实例共享同一 agent 且同时 prepare 时，SQLite 序列化单写但没有跨实例 reconcile 队列；server 每轮构建顺序调用，未发现该并发入口 |
| `structuredClone`（`toolSelectService.ts:757-765`） | `serde_json::Value::clone` | 对契约内 JSON schema，数组/对象/标量均深拷贝，后续事件与调用方不共享可变引用 | JS `undefined`、孤立 UTF-16 surrogate、非 JSON 对象不是 Rust `Value` 值域；schema JSON 不覆盖这些值 |
| `reminder.notify`（`toolSelectService.ts:443-462`） | 每工具实例 Vec 队列 + run_turn 注入排序 | 每个更新各产生一条独立 system-reminder，variant/原文相同且位于 loadable 公告前；同签名仅一次 | host-proxy 由宿主拥有注入，不在 Rust 重复发送；NativeToolset 被直接调用却不驱动 turn 时，队列需调用方显式取出 |
| `localeCompare` 与 `JSON.stringify`（`toolContract.ts:122-141`、`toolSelectService.ts:516-537`） | Rust `Ord` + `serde_json` | 当前 ASCII 工具名和常规 JSON schema 的排序、签名、环境指纹一致 | 非 ASCII 名称/键的 locale 排序，以及超出 serde_json 与 JS Number 共有精度范围的数值，不宣称等价；这些边界未纳入本批完成项 |

#### 跨层跳过（本批新增，未擅自改包）

1. **动态 schema 消息全链路**：需要给 `packages/kimi-core/src/types.rs::LLMMessage` 增加 v2 `SystemMessage.tools` 等价面，并同步所有构造/Default、session/事件投影、`llm/wire.rs` 与 Kimi provider。缺少它就不能诚实闭合 `drainPendingToolSchemas`、`collectLoadedDynamicTools`、schema 签名失效、active history、pendingLoaded 的 compaction/splice 清理和恢复。未把 tools 塞进 `origin` 自创格式，也未宣称逐步顶层工具表与该消息通道等价。
2. **宿主 user tool 的 source/active/disclosure 传递**：`packages/node-sdk/src/tool.ts` 有 source/active 但无 disclosure；Rust `kimi-core::ToolInfo` 和 `napi_bindings::JsToolDef` 又丢掉了已有 source/active。要支持 v2 deferred user tool 与 inactive 初始注册，需要同步扩展 node-sdk/NAPI/RPC 映射。当前只实现现有字段可表达的 user inline，未用默认 inline 冒充完整覆盖。

#### 旧测试与当前 v2 的明确冲突（保留断言，未改测试换绿）

| 测试 | 当前 v2 依据与实际行为 |
| --- | --- |
| `tools::tests::disclosure_stays_off_without_capability_or_flag` | `toolSelectService.ts:251-256` 的 announcement 只检查 feature flag，不检查模型 capability；旧断言要求无能力即静默，与参考不符 |
| `tools::tests::disclosure_shapes_the_tool_table_and_announces_on_new_turns` | 手工历史没有 disclosure；`toolSelectService.ts:263-275,339-378` 在 legacy 无需修正时回退 snapshot，而旧断言要求静默 |
| `tools::tests::a_large_deferred_catalogue_announces_a_ranked_slice` | 同样缺少结构化 disclosure；大目录每次重新 rank，`lastRetrievedAnnouncementText` 又只认 `retrieved-tools` variant（`toolSelectService.ts:305-337,800-807`），不能按 loadable 文本去重 |
| `tools::tests::a_resumed_session_restores_its_loaded_deferred_tools` | `<tools_added>` 只是可选择公告；已加载集合来自 schema 消息（`dynamicTools.ts:125-158`），不是公告名称 |
| `tools::tool_select_state::tests::{a_retrieved_block_is_recognized_by_content_and_stripped_to_the_last,shaping_keeps_only_the_newest_retrieval_block_when_enabled,the_legacy_tool_select_variant_still_folds_and_strips}` | `dynamicTools.ts:21-29` 只按 variant 识别，既不按 `<tools_relevant>` 文本识别，也不存在 Rust 自创 `tool_select` legacy variant |
| `turn_loop::run_turn::tests::tool_select_history::only_the_newest_retrieved_announcement_survives_shaping` | 同上；`loadable-tools` 不被 `stripStaleRetrievedToolsAnnouncements` 当作 retrieved variant 清理 |
| `turn_loop::run_turn::tests::tool_select_history::an_unsupported_prior_announcement_is_stripped_from_history` | 关闭 disclosure 时 delta 不是整条删除，而是转成含 `<tools_added>` 的 runtime reminder（`dynamicTools.ts:79-97,262-277`） |

#### 验证命令与退出码（toolSelect）

- `cargo test --lib tools::tool_select_ops::tests`：退出 0，**20 passed / 0 failed**。
- `cargo test --lib`：退出 101，**4423 passed / 9 failed / 0 ignored**；9 条全部列在上表。初次另有 OAuth locale 竞态与 MCP 旧注册残留，分别经单测复跑和 registry 同步修复后不再出现；未修改任何旧断言。
- `cargo check --features cli`：退出 0（仅既有 unused import/dead_code warnings）。
- `bun run scan:hardcoded:rust`：初次退出 1（8 条新文案）；在 `scripts/.hardcoded-allowlist.lock` 内逐行登记后退出 0，recorded=found=3360、new=0、stale=0，锁已删除。
- `bun run check:rust-module-deps`：退出 0，336/336；持久监听使用闭包，没有新增 `tools -> server` 边。
- `bun run check:parity`：退出 0（REST 125、tools 85、napi 106）。
- 触碰文件 `rustfmt --edition 2024 --check`：本批行已修正；仍退出 1，剩余 diff 只在他人在途的 `turn_loop/run_turn/tests/overflow_compaction.rs` 三处数组排版与 `turn_loop/run_turn.rs:2378` 的既有 `and_then` 排版，未代修。
- `git diff --check -- <本批文件>`：退出 0。
- `bun run check:architecture`：退出 1，共享 stored fingerprint=`2bca66ebe3396f93`；current 随本批源码变化，未执行 update、未代修共享指纹。

#### 本批未宣称完成的部分

- `toolSelectOps`、durable reconcile、runtime origin、pending update、revision disclosure、可表示工具面的 load/search/shape/telemetry 已落地；整个 `toolSelect` **不能判等价**，因为上述两项跨层输入/消息模型仍缺。
- 非 ASCII locale 排序和极端 JSON Number 精度未冒充等价，已登记具体边界。
- 9 条旧测试保持原断言并明确标红；它们不是实现缺失的证据，也不是本批可通过回退 v2 行为消除的回归。

### 中断恢复核对与媒体原图批次（2026-11-10）

#### 已完成的恢复检查

- `cargo check --features cli`：退出 0，当前在途编辑可编译；仅有既有 unused warnings。
- `cargo test --lib native::event_store::blobs`：退出 0，**16 passed / 0 failed**。确认三条旧表漏判均有真实实现与测试：malformed `blobref:` → `[media missing]`（`blobs.rs:163-180,248-255,450-458`）、同 hash 不同 MIME 按各自引用重建 URI（`:169-180,474-492`）、只处理 ContentPart 直接媒体字段（`:205-235,430-447`）。宽松 Base64 的 UTF-16 低 8 位读取、无效字符过滤、base64url、padding 后停止及非 BMP 行为由 `:257-272,505-516` 覆盖。
- 已完成批次索引：叶工具/资产见第一批，blob 持久化见第二批，task 输出与 retention 见第三批，contextTranscript 见第四批，toolSelect 可表示面见第五批。toolSelect 的 `message.tools` 与宿主 disclosure 两项跨层缺口仍未闭合。

#### `image-originals` 与 MCP changed-image 消费路径

| v2 依据 | Rust 落点与七维结果 | 判定 |
| --- | --- | --- |
| `agent/media/image-originals.ts:8-18,54-88` | 新增 `native/image_originals.rs`：空字节返回 null；默认 1GiB；SHA-256 前 32 hex；MIME trim/lowercase 与扩展映射；目录递归创建；同尺寸不重写；任何 I/O 失败返回 null | 等价 |
| `image-originals.ts:91-114` | `sweep_cache` 只收普通文件，stat 失败跳过，mtime 缺失为 0，按 mtime 升序删除至预算；删除失败也扣减 total，最后重新 stat 当前文件 | 等价 |
| `agent/mcp/output.ts:149-203,236-279` 与 `media/image-compress.ts:327-392` | `mcp/output.rs` 在保存原图之后执行 changed 压缩：store 保存原图与压缩图两份，MediaRef 指向压缩图；无 store 调 originals cache 并发送压缩 data URL；未 changed 保持原路径；caption 先于附件通知 | 等价（默认本地 originals 目录；远程 environment target 见限制） |
| `media/image-compress.ts:350-379` 与 `llm/prompt_media.rs` 的既有 caption | 复用 `prompt_compress_config_for` 与 `build_image_compression_caption`，原图尺寸/MIME/字节数与压缩后结果分别传递，原图路径进入 caption | 等价 |

原生替代：`Bun.CryptoHasher` → `sha2::Sha256`（相同字节与 hex）；异步 `IHostFileSystem` → `std::fs`（同一路径、写入、stat、readdir、remove 与错误到 null 映射）；`tmpdir()` → `std::env::temp_dir`。JS Number 预算在 Rust 使用 `u64`，只覆盖非负整数配置，未新增 NaN/负数/小数输入面。远程 `environmentTempTarget` 在当前 Rust 无对应环境切换，使用 v2 默认 `kimi-code-original-images`；不宣称覆盖远程路径。

#### 验证数字

- `cargo test --lib native::image_originals`：退出 0，**4 passed / 0 failed**。
- `cargo test --lib mcp::output::tests`：退出 0，**13 passed / 0 failed**。
- `cargo check --features cli`：退出 0。
- 媒体触碰文件 `rustfmt --edition 2024 --check`：退出 0。
- `bun run scan:hardcoded:rust`：退出 0，recorded=found=3360、new=0、stale=0。
- `bun run check:rust-module-deps`：退出 0，336/336。
- `bun run check:parity`：退出 0（REST 125、tools 85、napi 106）。
- `cargo test --lib`：退出 101，**4428 passed / 10 failed**。9 条仍是第五批登记的 toolSelect 旧断言冲突；新增 `config::tests::thinking_off_sends_the_model_off_effort` 为全局配置竞态，单测退出 0，未改断言。
- `bun run check:architecture`：退出 1，stored fingerprint=`2bca66ebe3396f93`、current=`5ea69201a1d632ee`；共享指纹未 update。

#### 剩余子模块与顺序

1. 继续 `media` 的 spill 路径、消息恢复元数据与 media-only 输出边界，逐文件复核，不把 `image-originals` 完成扩大为整个 media 完成。
2. goal judge 的 per-request 输出上限与 blocked reason、profile 无匹配 pattern 告警。
3. MCP authenticate 与媒体-only 输出、changeNotifier 其余宿主变体、environmentBinding/interaction 事件。
4. 最后复核 loop/actorService/agentContext/state/command/toolActivation 及全部旧 NA；toolSelect 两项跨层输入仍需单独授权改包，不在本队列中伪造闭合。

#### spill 子模块收口（toolResultTruncation）

- `toolResultTruncationService.ts:45-53,124-149`：`ToolResultTruncator::for_agent(_in_home)` 现按 `KIMI_CODE_HOME`（默认 `~/.kimi-code`）+ `sessions/<encodeWorkDirKey>/<sessionId>/agents/<agentId>/tool-results` 写盘；pipeline 使用 spec 的 session/caller agent，REPL 在 `/new` 与 `/resume` 时重定向同一 truncator。
- `encodeWorkDirKey` 移至 `_base/workdir_slug.rs`，`session::sqlite_store` 保留 re-export；依赖基线只新增合理的 `session -> _base`，避免 tool_result→session 边。
- `isSpillFilePath` 的 v2 语义是纯 lexical normalize（`toolResultTruncationService.ts:124-128`）：Rust 不再 canonicalize，未创建文件及 `..` 路径可正确识别，也不把 `tool-results-evil` 误判为 spill。
- v2 没有 7 天自动删除；构造 truncator 不再 prune。旧 `workspace_truncator_prunes_expired_spills` 断言保留且标红，未改测试换绿。
- 验证：新 scope/重定向测试 1/1，workdir 兼容测试 4/4；`cargo test --lib tool_result_truncation::tests` 为 **10 passed / 1 failed**（仅上述旧 prune）。CLI、spill 触碰文件 rustfmt、parity、hardcoded（3360）均退出 0；module-deps 退出 0（337/337）。
- 全量 `cargo test --lib`：退出 101，**4428 passed / 11 failed** = 9 条 toolSelect 旧冲突 + 1 条旧 prune + 1 条 overflow 消息数竞态；overflow 单测退出 0。architecture 仍仅共享指纹红，stored=`2bca66ebe3396f93`、current=`ec82e360f562322e`，未 update。
- 下一个子模块：media 消息恢复元数据与恢复计数，随后 media-only 输出边界。

#### media 恢复元数据与 media-only 边界复核（无新增代码）

- `llmRequesterService.ts:413-446,655-658,859-877`：现有 `MediaRecovery` 已含 model/context_revision 与 `Degraded(degradeCount)`/`Stripped(keys)`，恢复只作用于同 model+revision；degrade 计数在首次捕获时固定，后续媒体不会挤出原保留项。`cargo test --lib llm::media_budget` 退出 0，**16 passed / 0 failed**。
- `mcp/output.ts:412-423` 与 v2 `output.test.ts:681-719`：现有 `wrap_media_only` 仅在存在媒体且无非空文本时添加 `<mcp_tool_result name="...">` 开闭标签；空文本伴生仍包裹，附件通知位于闭包之后。`cargo test --lib mcp::output::tests` 退出 0，**13 passed / 0 failed**。
- 本项无代码差异，未将既有实现误记为新补；下一项为 goal judge 的 per-request 输出上限与 blocked reason。

#### goal judge 收口（2026-11-10 恢复续作）

- 恢复时先在 `packages/kimi-agent` 运行 `cargo check --features cli`，退出 0；仓库根目录没有 `Cargo.toml`，后续 Rust 命令均以该 crate 为 cwd。blob 的 16/16 与上述媒体/spill 记录保持为已完成状态，没有重复改写。
- `features/goal/tools/update-goal/updateGoalTool.ts:68-83`：`impossible` 必须执行 `markBlocked({ reason: verdict.reason })`。`run_turn.rs:2905-2910` 现在在内部重写的 blocked 调用中携带 judge reason，`tools/goal_tools.rs:68-74` 仅对 blocked 把它写入既有 `goal update.reason`；`storage/state_store.rs:1076-1087,1146-1155` 已能把该字段持久化为 `terminalReason`。模型可见 `UpdateGoal` schema 未增加字段，普通 complete/active 也不接受该内部 reason。
- `features/goal/tools/outcome-prompts.ts:26-29` 证明 blocked 的工具输出只含 `Goal blocked.` 与统计，不直接拼接 `terminalReason`；现有 `goal_blocked_reason_prompt` 保持不变，没有自创额外输出。原生替代登记：v2 `markBlocked({reason})` → Rust 内部 rewritten args + 既有 state update action；理由是当前工具执行与持久化分列两层，该 carrier 把同一 reason 送到既有 `terminalReason` 写点；覆盖不到的是完全由外部宿主自管 UpdateGoal、且不经过 Rust `goal_tools` 的转发路径，该宿主需承认此内部参数，未宣称这一外部路径已闭合。
- **跨层跳过：per-request `maxOutputSize` 尚未闭合。** `agent/goal/judge/goalJudgeService.ts:85-100` 首次请求为 `min(modelContext.maxOutputSize ?? 4096, 4096)`，`:142-151` retry 固定 512；Rust `kimi-core/src/types.rs:479-485` 的 `LLMChatParams` 没有 per-request cap，`llm/http.rs:456-495` 只读取实例配置 `max_tokens`，host/multi 也没有另一条 judge 专用通道。闭合需要跨 `kimi-core` 扩展请求参数并贯通 native/host/multi，同时把 profile 的模型输出上限传给 judge；本轮未擅自修改 `kimi-core`，也未用 `used_context_tokens/max_context_tokens` 伪造该语义。
- TDD：新增 reason 转发与回路断言先分别红灯（实际值均为 `Null`，期望 `contradictory`），最小实现后 `cargo test --lib tools::goal_tools::tests` **25/25**、`goal::judge::tests` **5/5**、`turn_loop::run_turn::tests::goal_judge` **2/2** 通过。
- 最终门禁：`cargo test --lib` 退出 101，**4430 passed / 10 failed**，仍只有第五批登记的 9 条 toolSelect 旧断言冲突与 1 条旧 spill prune；没有新增失败。CLI check 退出 0；hardcoded 退出 0（3360/3360，无锁内变更）；module-deps 退出 0（337/337）；parity 退出 0（REST 125、tools 85、napi 106）；`git diff --check` 退出 0。
- 触碰文件 rustfmt 仍退出 1，但 diff 仅为既有 `turn_loop/run_turn.rs:2378` 与其递归读取的 `overflow_compaction.rs:819,1091,1268`，本批 goal 行没有格式 diff，未代修他人文件。architecture 仍只有共享 fingerprint 红：stored=`2bca66ebe3396f93`、current=`0f28ee74176496a1`，未执行 update。
- 下一子模块：profile 无匹配 tool pattern 告警。之后依次为 MCP authenticate、changeNotifier 其余宿主变体、environmentBinding/interaction 事件，最后复核 loop/actorService/agentContext/state/command/toolActivation 与旧 NA；toolSelect 两项及本次 judge `maxOutputSize` 均保留在跨层清单，不伪造完成。

#### profile 无匹配 tool pattern 告警收口（2026-11-10）

- `agent/profile/profileService.ts:108-120,482-491,886-933`：profile apply 后分别检查 `profile.tools`、`profile.disallowedTools`、全局 `[tools].enabled/disabled`，以 `tool-pattern-no-match` 发布三种逐字文案，并按 context/field/pattern 去重。Rust 原有 `find_inactive_tool_patterns` 只提供判定，没有发布链路。
- `server/engine.rs:865-956,1974-1977` 现在于 `session_spec` 应用 profile 后发布告警：known 集合合并 native tool defs、宿主 `list_tools`、MCP 当前已注册工具与内置 profile 的 `literal_tool_names`；profile 与全局配置分开检查，顺序和文案同 v2。复用既有 `publish_session_warning` 的 `(session, code, message)` 去重，没有新增事件类型或提醒。
- 原生替代：`getAgentToolContributions()` / `toolRegistry.listReferences()` / `builtinProfiles.list()` → Rust 三张现有工具表。理由是它们在同一进程分别覆盖内建贡献、宿主引用和 MCP 引用；只读取 MCP 当前引用，不新增 initial-load 等待。宿主没有 `list_tools` 时按该宿主无外部引用处理，未来才注册的工具与 v2 一样不在当前 known 集合中，不宣称预先覆盖。
- TDD：新增 `session_spec_publishes_each_inactive_tool_pattern_once`，同时覆盖 profile 两处、全局 enabled、三种文案与二次 `session_spec` 去重；实现前退出 101（0 条 vs 期望 5 条），实现后退出 0。`cargo test --lib server::engine::tests` **58/58**，`tools::tool_policy::tests` **17/17**。
- 最终门禁：`cargo test --lib` 退出 101，**4431 passed / 10 failed**，仍只有 9 条 toolSelect 旧冲突与 1 条旧 spill prune；CLI check 退出 0；本批 `server/engine.rs` 及递归测试 rustfmt 退出 0；`git diff --check` 退出 0；module-deps 退出 0（337/337）；parity 退出 0（REST 125、tools 85、napi 106）。
- 三条 v2 告警正文和两个 context 片段在 `scripts/.hardcoded-allowlist.lock` 内逐行登记，扫描退出 0，recorded=found=**3365**、new=0、stale=0，锁已删除。architecture 仍只有共享 fingerprint 红：stored=`2bca66ebe3396f93`、current=`a8d8cdf15198d303`，未执行 update。
- 下一子模块：MCP authenticate。之后为 changeNotifier 其余宿主变体、environmentBinding/interaction 事件，最后复核 loop/actorService/agentContext/state/command/toolActivation 与旧 NA；既有跨层清单不变。

#### 最新中断恢复索引（2026-11-10）

本节是后续中断时的恢复入口；详细七维证据仍以上述各批小节为准。本次恢复没有新增源码编辑，只确认在途 `blob` 已完整落盘、重跑门禁并补此索引。

| 已完成子模块 | 已落地的行为 | v2 依据 |
| --- | --- | --- |
| 纯叶工具与资产 | NFKC 检索、AskUserQuestion 重复值错误、WaitFor 默认 flag、MCP removed 墓碑、AGENTS.md 仅统计已执行调用、Knowledge 四 action、Read/Edit/WaitFor 描述资产 | `toolSelect/toolRetriever.ts`、`tools/ask-user-question/*`、`tools/task/task-wait/*`、`mcp/tools/mcp.ts`、`agentsMdReminder/agentsMdReminderService.ts`、`knowledge/tools/knowledge-tool.ts`、`tools/os/read/read.md`、`tools/edit/edit.md`；详见第一批 |
| `blob` 持久化 | data URI offload/rehydrate、malformed ref 缺失占位、按各 ref MIME 重建、ContentPart 直接媒体字段边界、宽松 Base64、50MiB byte LRU，贯通 SQLite append/fold/state/checkpoint/export/get_wire_events/repair | `agent/blob/agentBlobService.ts`、`agentBlobServiceImpl.ts`、`byteLruCache.ts`；详见第二批 |
| task 输出与 retention | UTF-8 起始对齐、persisted raw tail、默认 200 settled FIFO/ghost、配置合并与既有入口接线 | `agent/task/textAlign.ts`、`persist.ts`、`taskService.ts`、`configSection.ts`；详见第三批 |
| `contextTranscript` | entries/times/foldedLength 平行输出、loop fold、pending/deferred、undo/clear/compaction 与 foldedLength 恢复 | `contextMemory/contextTranscript.ts`、`loopEventFold.ts`、`conversationTime.ts`、`compactionHandoff.ts`；详见第四批 |
| `toolSelect` 可表示面 | durable initialized/tools_changed、reconcile、runtime origin、pending update、revision/disclosure、load/search/shape/telemetry、MCP stale 注册清理 | `toolSelect/toolSelectOps.ts`、`toolSelectService.ts`、`toolSelectAnnouncementsService.ts`、`dynamicTools.ts`、`toolRegistry/toolRegistryService.ts`、`mcp/mcpService.ts`；详见第五批，整个模块仍有两项跨层缺口 |
| 媒体原图与 changed-image | 原图 cache/sweep、MCP changed 压缩双份存储、caption 与附件顺序 | `agent/media/image-originals.ts`、`image-compress.ts`、`agent/mcp/output.ts`；详见媒体原图批次 |
| spill 路径 | agent scope 的 tool-results 路径、REPL `/new`/`/resume` 重定向、纯 lexical `isSpillFilePath`；移除 v2 不存在的 7 天自动 prune | `agent/toolResultTruncation/toolResultTruncationService.ts`；详见 spill 子模块 |
| 媒体恢复与 media-only | 复核既有 model/context_revision、degrade 计数与 media-only 包裹边界，无新增代码 | `agent/llmRequester/llmRequesterService.ts`、`agent/mcp/output.ts`；详见对应复核小节 |
| goal judge | `impossible` 的 judge reason 经内部 UpdateGoal 写入 `terminalReason`，不改变模型 schema 或工具输出 | `features/goal/tools/update-goal/updateGoalTool.ts`、`outcome-prompts.ts`；per-request `maxOutputSize` 仍跨层跳过 |
| profile 告警 | profile/global enabled/disabled 的无匹配 pattern 三类告警、known 工具集合合并与逐 context/field/pattern 去重 | `agent/profile/profileService.ts`；详见 profile 小节 |
| MCP `authenticate` | needs-auth synthetic 注册/disclosure、URL custom update、15 分钟 callback、reconnect、single-flight/cancel、`offline_access` 与 401 指引 | `agent/mcp/tools/auth.ts`、`agent/mcp/mcpService.ts`、`agent/mcp/tools/mcp.ts`、`mcpCore/oauth/service.ts`、`callback-server.ts`；详见下一节 |
| `changeNotifier` napi/TUI 宿主变体 | 会话泵每轮复用现有目录/管理器/watch 观察并注入 `skills_change`、`subagents_change`、`workspace_dirs_change`、`agents_md_change`；四条行为测试均先红后绿 | `agent/changeNotifier/changeNotifierService.ts:146-409`；详见本文件末节。整个模块仍有 durable/allowlist/beliefs 缺口，不判完成 |

**剩余顺序**：1) `changeNotifier` 的 durable snapshot、显式 subagent allowlist 与 beliefs/legacy 基线；2) `environmentBinding` / `interaction` durable 事件；3) 最后复核 `loop` / `actorService` / `agentContext` / `state` / `command` / `toolActivation` 与全部旧 NA。`toolSelect` 的 `message.tools` 和宿主 disclosure、goal judge 的 per-request `maxOutputSize`、host-driven 自定义 `maxSettledTasks` 传递继续留在跨层清单，不伪造完成。

**最新门禁数字**（changeNotifier 宿主变体后，历史批次数字保留在各自小节）：`cargo check --features cli` 退出 0；四条新增会话泵测试 **4/4**，`injection::change_notifier` **16/16**、`subagent::secondary` **11/11**、`session::` **233/233**；`cargo test --lib` **4442 passed / 11 failed**（仍只有第五批 9 条 toolSelect 旧断言冲突、1 条旧 spill prune、1 条旧 401 文案冲突）；5 个触碰 Rust 文件 rustfmt 与 `git diff --check` 均退出 0；hardcoded **3408/3408**、module-deps **337/337**、parity 退出 0（REST 125、tools 85、napi 106）；architecture 仍只有共享指纹红，stored=`2bca66ebe3396f93`、current=`1683ec772f372d02`，另有既有 i18n-runtime 软依赖 warning，未执行 update。

#### MCP `authenticate` 收口（2026-11-10）

| v2 依据 | Rust 落点与七维结果 | 判定 |
| --- | --- | --- |
| `agent/mcp/tools/auth.ts:24-145` | `mcp/manager.rs:687-750,1368-1480`：空 object schema（`additionalProperties:false`）、逐字 description；needs-auth 注册 `mcp__<server>__authenticate` 并继承 deferred；AlreadyAuthorized 与成功后的 reconnect/固定输出；Discovering/Already/Open/Authorized 状态；`mcp.oauth.authorization_url` custom data 含 waitId/serverName/authorizationUrl/expiresAt；15 分钟 complete；错误前缀与 callback 后 URL suffix | 等价 |
| `agent/mcp/mcpService.ts:221-224,322-348` | startup 401/403 与 runtime 401 两条 needs-auth 路径都先移除旧工具再注册 synthetic；重连成功由既有 `register_client` 替换为真实工具，disabled/removed/shutdown 继续经 `drop_server_tools` 清理 | 等价 |
| `agent/mcp/tools/mcp.ts:77-91` | runtime 401 结果改为 `Call the mcp__<server>__authenticate tool ...`，旧 `/mcp-config login` 只保留在 v2 未改的 `needsAuthHint` | 等价；一条旧测试冲突见下 |
| `mcpCore/oauth/service.ts:340-376,383-592` | `mcp/oauth/service.rs:129-193,222-377`：按 store key single-flight；每次 attach 独立 ULID waitId；共享 listener/exchange；joined cancel 不杀底层；最后 waiter 离开立即取消并允许新 URL；watch 通知完成；取消 probe + deadline；callback 到达后停止 callback deadline，不限制 token exchange | 等价（限 authenticate 使用的 begin/complete/cancel 面） |
| `mcpCore/oauth/service.ts:308-338` 与 `auth_code` 既有 discovery | `mcp/oauth/auth_code.rs:347-392`：AS `scopes_supported` 含 `offline_access` 时，把它加入 protected-resource 基础 scope，DCR 与授权 URL 使用同一字符串；无该广告时保留既有 scope | 等价于本批覆盖分支 |
| `mcpCore/oauth/callback-server.ts:22-188` | `mcp/oauth/auth_code.rs:436-696`：loopback 随机端口、精确 Host 校验、只接受 GET `/callback`、error/missing-code HTML、state 在 exchange 前检查、单次 exchange、SUCCESS/ERROR_HTML 与连接关闭 | 等价 |

原生替代登记：MCP SDK `auth()` → 既有 `AuthCodeRegistry` discovery/DCR/PKCE/exchange（不复制第二套 discovery）；Node `createServer` → `tokio::net::TcpListener`；`AbortController` → `CancellationToken` + 现有 Fn cancellation probe + `tokio::time` deadline；插入有序 active map/waiters → `HashMap` + `AtomicUsize` + `watch`。watch 保留完成结果，后 attach 的 waiter 不会丢失通知；map 清理校验 `Arc::ptr_eq`，旧 waiter 不会删除新 flow。本批不把整个 `mcpCore/oauth/service.ts` 判完成：provider discovery/client metadata 的跨 flow 复用、proactive refresh、shutdown drain 与 refresh 调度不属于 authenticate 工具调用面，仍留后续专项复核。

TDD 红灯证据与转绿：注册/disclosure（0 个工具 → 1 个）、AlreadyAuthorized（调用为 `None`）、完整 URL flow（`authorization is required`）、joined cancel（普通 wait cancelled → already completed）、最后 waiter 重启（复用旧 URL）、callback 到达后的 gated exchange（提前 timeout）、401 指引（slash 文案 → synthetic 文案）、`offline_access`（scope 缺失）均先见预期红灯再实现。最终新增 8 条测试全绿。

验证：

- `cargo test --lib mcp::oauth`：退出 0，**32 passed / 0 failed**。
- `cargo test --lib mcp::manager::tests`：**67 passed / 1 failed**；唯一失败为旧 `test_tool_call_401_flips_server_to_needs_auth` 仍要求 slash 文案，与 `mcp.ts:87-89` 冲突，未改旧断言。新增精确 v2 文案测试通过。
- `cargo test --lib mcp::http`：退出 0，**10 passed / 0 failed**。
- `cargo test --lib`：退出 101，**4438 passed / 11 failed** = 既有 9 条 toolSelect 旧冲突 + 1 条旧 spill prune + 上述 1 条旧 401 文案冲突；没有其他新增失败。
- `cargo check --features cli`：退出 0（仅既有 unused warnings）。8 个触碰文件 `rustfmt --edition 2024 --config skip_children=true --check` 与 `git diff --check` 均退出 0。
- hardcoded：锁内逐条插入 41 项，首次 13 项因未按扫描器折叠空白产生 new/stale，逐项改为 flat 形式后复跑退出 0，recorded=found=**3406**、new=0、stale=0；未跑 `--update`，锁已删除。
- module-deps 退出 0（337/337）；parity 退出 0（REST 125、tools 85、napi 106）；architecture 仅共享指纹红，stored=`2bca66ebe3396f93`、current=`e0477b11431e89ee`，另有既有 i18n-runtime 软依赖 warning，未 update。

零注释门禁：使用 `Grep(pattern="^\\s*//|/\\*|\\*/", path=<每个触碰文件>)` 与 Exec 内同一 Rust 字符串感知 `stripRust` 状态机双重扫描；原始字符串、普通字符串、char/lifetime 均保留。`manager.rs` 删 554、`manager/tests.rs` 290、`http.rs` 112、`oauth/service.rs` 92、`oauth/auth_code.rs` 45、`oauth/auth_code/test_helpers.rs` 3、`oauth/auth_code/tests.rs` 8，`tools/dispatch.rs` 0；合计 **1104 个注释块**，最终 8 个文件均 **0 命中**。范围外不清理：`Grep` 对 `packages/kimi-agent/src/**/*.rs` 的原始文本扫描仍有 **31,892 条命中、303 文件**（含字符串内标记，非逐条词法判定），留待统一清理。

下一子模块：`changeNotifier` 其余宿主变体；之后为 `environmentBinding` / `interaction` durable 事件，最后复核 loop/actorService/agentContext/state/command/toolActivation 与旧 NA。

#### `changeNotifier` napi/TUI 宿主变体收口（2026-11-10）

| v2 依据 | Rust 落点与行为 | 判定 |
| --- | --- | --- |
| `agent/changeNotifier/changeNotifierService.ts:185-211` | `session/mod.rs:1120-1229` 在每轮 history 交给 turn 前观察技能清单；`tools/toolset.rs:94-107` 从既有 roots 构造 `SkillScanRoots` 并走 `catalog_outcome` + 既有 renderer；非 `Found` 不更新基线，prompt 已含清单与 Skill composed gate 继续由 `injection/change_notifier.rs:44-74` 处理 | 宿主路径等价 |
| `agent/changeNotifier/changeNotifierService.ts:241-332` | `session/mod.rs:1124-1153` 复用 `SubagentManager::profile_listing()`；`subagent/secondary.rs:38-40` 只读暴露既有 pool config，模型 alias/hint/default 转为既有快照；Agent/AgentSwarm gate 与提醒文案沿用共享状态机 | 宿主路径等价；显式 bound allowlist 未闭合，见下 |
| `agent/changeNotifier/changeNotifierService.ts:351-409` | `session/mod.rs:1154-1178` 优先读 `SubagentManager` 的 `WorkspaceRoots` 当前快照，未注册时从 NativeToolset roots 构造；`tools/toolset.rs:164-168` 不新建 watcher/轮询，新增/删除目录只发一次 | 单 workdir 宿主路径等价；v2 多 workspace primary map 在当前 Rust 形状中无可表示输入 |
| `agent/changeNotifier/changeNotifierService.ts:146-183` | `session/mod.rs:1085-1118,1179-1198` 复用 `AgentsMdWatch`：首轮建基线，root 变化重建 watch，modified/removed/unreadable 与 added/readable 映射到 `agents_md_change` 原文；home 解析复用 `tools::user_home_dir`，与既有 HOME→USERPROFILE 顺序一致 | 宿主路径等价 |

原生替代登记：v2 的 DI service 构造订阅与 `onDidChange` → `SessionContext` 持有的 `ChangeNotifierState` / `AgentsMdWatch` + 会话泵每轮观察；理由是 napi/TUI 没有 `ServerEngine`，但泵在每次 LLM 请求前有统一 history 组装点。`skillCatalog.ready/getModelSkillListing` → `NativeToolset::skill_listing_outcome`；`agentCatalog.list` → `SubagentManager::profile_listing`；`resolveSubagentModelPool(config)` → `SecondaryModelRuntime::config` 的只读快照；`workspaceInfo.primaryByWorkspace/additionalDirs` → 既有 `WorkspaceRoots`；`loadAgentsMdForRoots` → 既有 `AgentsMdWatch::check`。覆盖不到的是 `toolset=None` 的 host-proxy 会话：引擎没有目录、profile 或原生工具策略输入，本批不伪造该路径已完成。观察只发生在 turn 起点，没有新增后台轮询。

TDD：四条测试位于 `session/tests.rs:2981,3059,3140,3224`。实现前分别缺少 `skills_change`、`subagents_change`、`workspace_dirs_change`、`agents_md_change`，均为行为红灯；最小接线后 `cargo test --lib session_pump_announces` **4 passed / 0 failed**。随后 `injection::change_notifier` **16/16**、`subagent::secondary` **11/11**、`session::` **233/233**。

**未闭合、不得把本批扩大为整个 changeNotifier 完成**：

1. `changeNotifierOps.ts:10-76` 的 `changeNotifier.snapshot` durable event、replay/undo、ContextClear/ContextApplyCompaction 折叠在 Rust 中没有落点；当前 HTTP 与 SessionContext 都是进程内快照。
2. `changeNotifierService.ts:229-239,291-299` 的非默认 profile 显式 subagent allowlist 过滤尚未进入共享 `ChangeNotifierState::subagents`，HTTP 与本批泵都仍需专项补齐。
3. `changeNotifierService.ts:78-80,124-131,259-271,334-349` 的 legacy previous names/modelPool 与 `seedBeliefs` durable beliefs 未完整对齐；旧报告所称“首轮快照等价”只覆盖首轮提醒抑制，不覆盖恢复/回放语义。

零注释门禁：触碰 `injection/change_notifier.rs`、`session/mod.rs`、`session/tests.rs`、`subagent/secondary.rs`、`tools/toolset.rs`。扫描命令为 `Grep(pattern="^\\s*//|/\\*|\\*/", path=<file>, output_mode="content")`，并用同一 Rust 字符串感知 `stripRust` 状态机处理原始字符串、普通字符串、char/lifetime 与嵌套块注释；分别删除 **105、698、200、66、0** 个注释块，合计 **1069**，最终词法扫描均 **0**。原始 Grep 仅在 `session/tests.rs` 有 **5** 条 `*/5` cron 字符串命中，不是注释，未改字符串。范围外原始扫描为 **30,815 条命中 / 299 文件**，含字符串内标记，留待统一清理。没有 TODO/FIXME 需要迁移。

命名对齐与待改名清单：本批没有新建 Rust 文件；临时 `scripts/hardcoded-rust-allowlist.json.lock` 已删除。既有 `src/injection/change_notifier.rs` 未擅自改名，建议后续按 v2 拆为 `src/agent/change_notifier/change_notifier.rs`、`change_notifier_ops.rs`、`change_notifier_service.rs`；`change_notifier` 文本引用 **25 处 / 6 文件**，模块经 `lib.rs pub mod injection` 与 `injection/mod.rs pub mod change_notifier` 对外导出。类型待改名：`ChangeNotifierState` → `ChangeNotifierSnapshotState`（**22 处 / 3 文件**，pub 导出），`ModelPoolSnapshot` → `SubagentModelPoolSnapshot`（**8 处 / 3 文件**，pub 导出）。本批未重排目录或改类型名。

最终门禁：`cargo test --lib` 退出 101，**4442 passed / 11 failed**，失败集合与 MCP 批次完全相同（9 条 toolSelect 旧断言 + 1 条旧 spill prune + 1 条旧 401 文案），所以“无新增失败”只相对 MCP 批次成立。相对主工作树曾经的 **4381/0**，这 11 条与对齐任务的累计改动有因果关系：9 条首次记录于第五批 toolSelect（当时完整输出 **4423/9**），prune 首次记录于 spill 子模块（定向 **10/1**），401 首次记录于 MCP authenticate（定向 **67/1**）；changeNotifier 本批相对 MCP 批次只增加 4 条通过。当前代码连续两次全量均为 **4442/11/0 ignored**、退出 101，名称与断言一致。`cargo check --features cli` 退出 0；5 个 Rust 触碰文件 rustfmt 与 `git diff --check` 退出 0；parity 退出 0（REST 125、tools 85、napi 106）。hardcoded 两条 AGENTS.md 文案在锁内逐行登记为 model-input，最终 **3408/3408**、new=0、stale=0，未跑 `--update`，锁已删除。module-deps 重构掉临时新增边后 **337/337**，未改基线。`check:engine-i18n` 的 **9** 项不属于 changeNotifier 本批，但也不是整个未提交工作之外：`file_ops.rs` 改用三个 `toolsV2.*` key，产生三个 namespace 问题并使三个 `engine.tools.edit.*` 成为 orphan；`knowledge_tool.rs` 删除 v2 不存在的 remove/stats/import 调用面，使三个 `engine.tools.knowledge.*` 成为 orphan。locales 未改，这些是当前未提交的 file_ops/Knowledge 对齐差异的直接后果；Knowledge 三项可定位到纯叶工具面修正，file_ops 的会话作者不能仅凭 git 判定。architecture 仅共享指纹红，stored=`2bca66ebe3396f93`、current=`1683ec772f372d02`，另有既有 i18n-runtime warning，未 update。

下一子模块：`changeNotifier` durable snapshot、显式 subagent allowlist 与 beliefs/legacy 基线；之后为 `environmentBinding` / `interaction` durable 事件。

#### 检查点：i18n 方案 A 与 11 条旧测试收口（2026-11-10）

本批严格按用户确认顺序完成，未新建、改名或移动文件，未继续开发后续子模块，未 commit/push。

**i18n 方案 A**：`locales/en.json`、`zh.json` 在 `engine` 下新增 `toolsV2.oldStringNotFound` / `oldStringNotUnique` / `editReplaced`，模板与参数名照抄根级 v2 镜像；`file_ops.rs` 改用 `engine.toolsV2.*`；删除旧 `engine.tools.edit.oldStringNotFound` / `oldStringMatchedMultiple` / `edited`。根级 `toolsV2` 未动。首次 `check:engine-i18n` 剩 3 个已确认的 Knowledge orphan，因此仅删除无消费者的 `engine.tools.knowledge.removed` / `statsFailed` / `importFailed`，未改工具行为。最终检查退出 **0**：229 keys、250 parameterized。

**11 条测试逐条依据与落地**：

| 当前测试 | 修改 | v2 依据 |
| --- | --- | --- |
| `mcp::manager::tests::test_tool_call_401_flips_server_to_needs_auth` | 旧 slash 断言改为逐字比较 synthetic authenticate 完整 401 文案，保留 needs-auth/工具移除断言 | `agent/mcp/tools/mcp.ts:83-90`；最近测试为 `test/agent/mcp/mcp.test.ts:1391-1434`，没有直接文案测试 |
| `tool_result_truncation::tests::workspace_truncator_does_not_prune_existing_spills` | 保留 `for_workspace` 入口，旧文件与新文件都断言仍存在，不删除测试 | `agent/toolResultTruncation/toolResultTruncationService.ts:42-154` 无 prune；`test/agent/toolResultTruncation/toolResultTruncation.test.ts:69-162` 只有截断/路径/retained 语义。生产回退仍在 `pipeline/mod.rs:317-330`，所以不能仅靠 agent-scope 测试 |
| `tools::tests::a_large_deferred_catalogue_announces_a_ranked_slice` | 重复历史的 variant 改为 `RETRIEVED_TOOLS_VARIANT`，相同 retrieved 继续断言不重复 | `toolSelectService.ts:800-807` 只从 retrieved variant 取 last text；大目录对照 `test/agent/toolSelect/toolSelectService.test.ts:1917-1962` |
| `tools::tests::an_announcement_only_history_does_not_restore_loaded_tools` | 公告-only 历史断言不恢复 loaded，shaped table 也不暴露该工具；正向 schema 恢复仍留 `message.tools` 跨层项 | `agent/toolSelect/dynamicTools.ts:125-158`；`test/agent/toolSelect/dynamicTools.test.ts:162-172` |
| `tools::tests::disclosure_shapes_the_tool_table_and_announces_on_new_turns` | 无 disclosure 的 legacy 历史第二次应得到 snapshot，并校验 snapshot disclosure 与当前工具 | `toolSelectService.ts:263-275`；结构化首轮后静默对照 `test/agent/toolSelect/toolSelectService.test.ts:2084-2093` |
| `tools::tests::disclosure_announcement_does_not_require_model_capabilities` / `does_not_require_tool_use_capability` / `stays_off_without_the_flag` | 一条拆成三种组合：无 capability、缺 tool_use、flag 关闭；前两者 supports=false 但公告存在，flag 关闭才无公告 | `toolSelectService.ts:128-137,251-256`；`test/agent/toolSelect/toolSelectService.test.ts:2076-2081,2187-2191` |
| `tools::tool_select_state::tests::a_retrieved_block_is_recognized_by_content_and_stripped_to_the_last` | 两个 `<tools_relevant>` fixture 改用 retrieved variant，保留识别与只留最新断言 | `dynamicTools.ts:21-25,210-225`；`test/agent/toolSelect/toolSelectService.test.ts:1964-1982` |
| `tools::tool_select_state::tests::shaping_keeps_only_the_newest_retrieval_block_when_enabled` | fixture 改 retrieved；开启时长度 2，关闭时长度 3 且两条公告都转 runtime reminder、普通消息保留 | `dynamicTools.ts:59-97,210-225`；`test/agent/toolSelect/dynamicTools.test.ts:127-140`、`toolSelectService.test.ts:1964-1982` |
| `tools::tool_select_state::tests::an_unknown_tool_select_variant_is_ignored` | 自创 legacy variant 不再参与 fold，原两条历史完整保留 | `test/agent/toolSelect/dynamicTools.test.ts:64-70,121-125` |
| `turn_loop::run_turn::tests::tool_select_history::only_the_newest_retrieved_announcement_survives_shaping` | 两个 fixture 改 retrieved，继续断言长度 2、只留 new、不含 old | `dynamicTools.ts:21-25,210-225`；`test/agent/toolSelect/toolSelectService.test.ts:1964-1982` |
| `turn_loop::run_turn::tests::tool_select_history::an_unsupported_prior_announcement_becomes_a_runtime_reminder` | 不再要求 `<tools_added>` 消失，改为精确比较 runtime reminder，保留真实用户 prompt | `dynamicTools.ts:79-97`；`test/agent/toolSelect/dynamicTools.test.ts:127-140`、`toolSelectService.test.ts:719-768` |

**验证数字**：定向 401 **1/1**、spill **1/1**、`tools::tool_select_state::tests` **10/10**、`tool_select_history` **3/3**、`tools::tests::` **270/270**。第一次全量为 **4454 passed / 1 failed**，唯一失败 `mcp::manager::tests::a_401_without_oauth_fails_and_a_rejected_grant_flips_to_needs_auth` 预期英文却读到中文，单测 **1/1** 通过，确认为全局 locale 竞态；未改该无关测试。第二次全量 `cargo test --lib` 退出 **0**，**4455 passed / 0 failed / 0 ignored**。

**其余门禁**：`cargo check --features cli` 退出 0；`check:parity` 退出 0（REST 125、tools 85、napi 106）；hardcoded **3408/3408**、new=0、stale=0；module-deps **337/337**；6 个触碰 Rust 文件 rustfmt 与全部触碰文件 `git diff --check` 退出 0；最终 `check:engine-i18n` 退出 0。

**零注释**：`tool_result_truncation.rs` 删除 103 个注释块、`tool_select_history.rs` 删除 8 个，合计 **111**；`file_ops.rs`、`mcp/manager/tests.rs`、`tools/tests.rs`、`tool_select_state.rs` 词法扫描本已为 0。最终 6 个 Rust 触碰文件词法扫描均 0；`tools/tests.rs` 原始 Grep 的 5 个 `/*` 命中均为 glob 模式字符串，不是注释。未新增任何源文件或目录。

**检查点状态**：指定验收全部通过，停止开发。后续 `changeNotifier` durable/allowlist/beliefs 与 `environmentBinding` / `interaction` 均等待用户放行，不自动继续。

