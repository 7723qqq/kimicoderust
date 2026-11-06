# 死代码盘点报告（第一阶段：只盘点，不删除）

> **已归档（2026-11-06）**：本文件是 2026-11-02 的历史快照，不再维护、不再作为裁决依据。
> 其中仍然有效的结论已并入 `packages/kimi-agent/ROADMAP.md`（新基点）的承接项。

- 盘点时间：2026-11-02
- 仓库基线：`main @ b2c89a8fe`，工作区含大量未提交改动（已逐一对照 `git status`，本报告未触碰任何文件）
- 口径：只盘点、不删除；所有「正在接线」资产单独标注

## 工具与方法（全部真实工具产出，非目测）

| 手段 | 覆盖 | 结果 |
|---|---|---|
| `cargo clippy --all-targets`（packages/kimi-agent） | Rust 全 crate（lib + bin + tests） | 0 条 `dead_code`/`unused` 告警；仅 4 条 `await_holding_lock`（均为 [run_turn/tests.rs](file:///G:/kimirust/packages/kimi-agent/src/turn_loop/run_turn/tests.rs#L7462) 里刻意的测试串行锁，非死代码） |
| `#[allow(dead_code)]` 全量 grep | Rust | 15 处，逐个读源码定性（见 §2），**全部不可删** |
| lib.rs 模块引用计数（`crate::<mod>` grep） | 36 个 `pub mod` | 无未接线模块（`knowledge`、`project_local_config` 等低计数模块均已确认有真实消费者） |
| Cargo 依赖 × 源码引用计数 | 41 个依赖 | 3 个零引用依赖（见 §3） |
| 自写 TS 导入图脚本（解析 import/export-from/dynamic import/require，含各包 tsconfig paths 别名、`#/`、`@/`、workspace 包名、package.json exports 含通配符、`.js→.ts` 映射） | 2049 个文件 | 孤儿文件候选 90 → 逐个人工复核后见 §4 |
| 导出符号 × 全仓单词索引（含 .vue/.d.ts 作为使用方，排除「仅本文件内复用」如 `z.infer<typeof x>`） | 同上 | 值导出零引用 102 条（§5 附录 A）、类型导出零引用 195 条（附录 B，低置信） |
| napi 契约 | napi-contract.d.ts vs Rust `#[napi]` | 由既有门禁 `check:parity` 覆盖且在 `check:all` 中通过，未发现孤立契约条目 |

## 一、结论摘要

| 类别 | 数量 | 说明 |
|---|---|---|
| 波次 1 · 可立即安全删除（与在跑改动无交集） | **16 个文件 + 3 个 Cargo 依赖** | vscode webview 13 个文件、vis 1 个、telemetry 1 个、napi-debug.mjs；`dialoguer`/`tower-lsp`/`lsp-types` |
| 波次 2 · 需等待（位于活跃改动区域） | **4 个文件 + 若干文件内死导出** | 均在 apps/kimi-code、packages/kimi-agent、packages/node-sdk、packages/protocol（当前未提交改动的同一批目录） |
| 波次 3 · 不可删（动态引用 / 预留 / 契约面） | 15 处 Rust allow(dead_code) + 9 个 workflow js + examples/bench 入口 + kosong/oauth/protocol 导出面 + kimi-web 整包 | 见 §6 |

**最值得先删（波次 1，全部零引用已逐条核实）**：

1. `apps/vscode/webview-ui/src/components/ui/` 下 12 个 shadcn 组件（accordion/badge/card/checkbox/combobox/command/context-menu/field/scroll-area/select/switch/tabs）+ `components/index.ts` 桶文件 — 全仓零引用（含测试）
2. `apps/vis/web/src/components/shared/Drawer.tsx`（50 行）— 零引用
3. `packages/telemetry/src/context.ts`（27 行）— 零引用，且不在 package.json exports 中
4. `packages/kimi-agent/Cargo.toml` 第 73/75/76 行的 `dialoguer`、`tower-lsp`、`lsp-types` — src/tests/build.rs 零引用

**需要你裁决的点**（见 §7）：wire-schema.ts 18 个未接线 schema（接上还是删）、kosong/oauth 公开导出零消费、kimi-web 未用 Vue 组件、vscode shadcn 组件是否作组件库保留、minidb bench / node-sdk examples 去留。

## 二、Rust：`#[allow(dead_code)]` 15 处 — 全部不可删

逐条读过源码，全部是「为上游 parity 预留 / 契约保留 / 仅 Drop 有意义」的刻意设计：

| 位置 | 定性 |
|---|---|
| [config/mod.rs:1310](file:///G:/kimirust/packages/kimi-agent/src/config/mod.rs#L1310) `resolve_compaction_trigger_ratio` | 配置 parity 预留，引擎尚未读取（注释明示） |
| [config/mod.rs:1323](file:///G:/kimirust/packages/kimi-agent/src/config/mod.rs#L1323) `resolve_reserved_context_size` | 同上 |
| [native/bash_spawn.rs:38](file:///G:/kimirust/packages/kimi-agent/src/native/bash_spawn.rs#L38) `BashSpawnEventKind::Error` | wire 契约保留变体 |
| [napi_bindings/session.rs:109](file:///G:/kimirust/packages/kimi-agent/src/napi_bindings/session.rs#L109) `mcp_status` | 字段为 Drop 语义存在（session 销毁即退订） |
| [rpc/types.rs:66](file:///G:/kimirust/packages/kimi-agent/src/rpc/types.rs#L66) `JsonRpcNotification` | JSON-RPC 协议类型 |
| [rpc/types.rs:772](file:///G:/kimirust/packages/kimi-agent/src/rpc/types.rs#L772) `RunTurnParams` / [:1004](file:///G:/kimirust/packages/kimi-agent/src/rpc/types.rs#L1004) `CancelTurnParams` / [:1253](file:///G:/kimirust/packages/kimi-agent/src/rpc/types.rs#L1253) `ToolExecuteResponse` | RPC 契约类型，字段经 serde 反序列化动态消费 |
| [swarm/service.rs:310](file:///G:/kimirust/packages/kimi-agent/src/swarm/service.rs#L310) `emit_suspended` | 注释明示被 `member_suspended` 取代，为既有构造点保留 |
| [native/path_access.rs:281](file:///G:/kimirust/packages/kimi-agent/src/native/path_access.rs#L281) `GuardMode::Disabled` | v2 parity 枚举臂 |
| [native/native_tool_bindings.rs:503](file:///G:/kimirust/packages/kimi-agent/src/native/native_tool_bindings.rs#L503) `native_list_directory` | `#[napi]` 导出，JS 侧动态调用 |
| [server/remote_control.rs:64-68](file:///G:/kimirust/packages/kimi-agent/src/server/remote_control.rs#L64-L68) 3 个握手常量 | 注释明示为 parity 保留（tokio-tungstenite 直接抛错） |
| [tools/swarm_tool.rs:43](file:///G:/kimirust/packages/kimi-agent/src/tools/swarm_tool.rs#L43) `AgentSwarmToolInput` | 工具入参 schema，serde 动态消费 |

## 三、Rust：未使用的 Cargo 依赖（3 个）

证据：`packages/kimi-agent/src`、`tests`、`build.rs` 全文 grep 均为 0 次出现。

| 依赖 | Cargo.toml 行号 | 备注 |
|---|---|---|
| `dialoguer` | 73 | 零引用 |
| `tower-lsp` | 75 | 零引用（疑似 LSP 功能未移植的遗留） |
| `lsp-types` | 76 | 零引用（同上） |

> 删除方式：删 Cargo.toml 三行 + `cargo check` 即可，无需 `cargo machete`。Cargo.toml 当前不在未提交 diff 中，但 kimi-agent 是另一会话主战场，建议波次 1 末尾或波次 2 处理并先知会。

## 四、TS：孤儿文件复核结论

导入图初筛 90 个候选，逐个复核后分类如下。

### 4.1 真死（零引用，可删）

| 文件 | 证据 | 波次 |
|---|---|---|
| `apps/vscode/webview-ui/src/components/ui/{accordion,badge,card,checkbox,combobox,command,context-menu,field,scroll-area,select,switch,tabs}.tsx` | 全仓（含测试、含 `@/`、`shared/*` 别名）零 import | 1 |
| `apps/vscode/webview-ui/src/components/index.ts` | 桶文件零引用（各业务组件被直接按路径引用， barrel 无人用） | 1 |
| `apps/vis/web/src/components/shared/Drawer.tsx` | 零引用（`IssuesDrawer` 是另一组件，不 import 它） | 1 |
| `packages/telemetry/src/context.ts` | 零引用；index.ts 不 re-export；node-sdk 自定义了同名类型而非引用它 | 1 |
| `apps/kimi-code/src/cli/workspace-preheat.ts` | `preheatWorkspaceIndex` 全仓仅定义处出现（含测试目录） | 2 |
| `apps/kimi-code/src/constant/index.ts` | 无 `from '#/constant'` 裸导入（30 处全部走子路径） | 2 |
| `apps/kimi-code/src/tui/components/index.ts` | 无 `from '#/tui/components'` 裸导入 | 2 |
| `apps/kimi-code/src/tui/components/dialogs/coding-plan-config.ts` | `CodingPlanConfigComponent` 全仓仅定义处出现；上游 0.30.0 合并后无人实例化 | 2 |
| `packages/node-sdk/src/v2/global-mcp.ts` | 无任何 import（`sdk-rpc-client-v2.ts` 存在但未引用它；其 4 个导出全仓零出现） | 2 |
| `packages/kimi-agent/napi-debug.mjs` | 手动调试脚本，零引用 | 2 |

### 4.2 看似孤儿但实际是入口 / 动态加载 — 不可删

| 文件 | 接线方式 |
|---|---|
| `packages/kimi-agent/src/workflow/builtin/*.js`（9 个） | Rust 侧 [workflow/registry.rs](file:///G:/kimirust/packages/kimi-agent/src/workflow/registry.rs#L15) `BUILTIN_SCRIPTS` 内嵌加载 |
| `packages/normify/src/cli.ts` | 根 package.json `check:normify` 直接以 `bun packages/normify/src/cli.ts` 调用 |
| `packages/normify/src/plugin-bin.ts` | `tsdown.plugin.config.ts` 构建入口 |
| `packages/kimi-agent/index.js` / `index.native.cjs` | napi-rs 自动生成的加载器（构建产物，勿手删） |
| `packages/minidb/src/worker/text-build-worker.ts` | `apps/kimi-code/tsdown.worker.config.ts` 以字符串路径引用打包 |
| `packages/node-sdk/examples/*.ts`（17 个）、`packages/oauth/examples/*.ts`、`packages/minidb/bench/*.ts`（17 个） | 手动运行的 smoke/bench 入口 |
| `packages/pi-tui/native/win32/build.mjs` | 构建脚本 |
| `packages/kosong/src/{inspection,request-trace,message-helpers}.ts`、`errors/{errorMessage,unexpectedError}.ts` | 经 package.json exports 子路径对外导出（公开 API 面），仓内零消费 — **见裁决点 2** |
| `packages/oauth/src/device.ts` | exports 的 `./device` 浏览器安全入口（文件头注释明示面向浏览器宿主），仓内零消费 — **见裁决点 2** |

### 4.3 正在接线 — 明确不可动

- **`packages/tree-sitter-bash/`（整包）**：另一会话正在接入 AGENTS.md 操作数目录。此刻的引用信号：未跟踪文件 [bash-target-dirs.ts](file:///G:/kimirust/packages/node-sdk/src/native/sdk-client/bash-target-dirs.ts) 已引用它，node-sdk/package.json 已声明依赖。**严禁提议删除。**
- 同理，未跟踪的 `packages/kimi-agent/src/session/lock.rs`、`packages/kimi-agent/src/subagent/allowlist.rs` 为在跑批次产物，不在盘点删除范围。

### 4.4 kimi-web（特例）

`apps/kimi-web` 被 workspaces 排除且 dist-web 为不可重建的预构建产物。其 `components/ui/*.vue` 12 个 + `chat/OpenInMenu.vue` 在 kimi-web 源码内零引用，但该包源码是否仍为准绳需你裁决（见裁决点 3），本报告不将其计入可删。

## 五、TS：零引用导出（值 102 / 类型 195）

方法：导出符号名在全仓（含 .vue/.d.ts/scripts）的单词出现 ≤1（即仅定义处），且排除本文件内复用（`z.infer<typeof x>` 等）。抽查 10 个值导出全部确认仅定义处出现，精度良好；但**字符串派发（如按 provider 名查表）无法被静态排除**，第二阶段删除前仍需逐条确认。

完整清单见附录。分布：apps/kimi-code 14 条值、packages/node-sdk 21 条、packages/kimi-agent(wire-schema) 13 条、packages/protocol 13 条值 + ~150 条类型、packages/transcript 3 条、packages/kosong 9 条、apps/kimi-web 9 条、其余散落。

重点提示：

- `packages/kimi-agent/wire-schema.ts`：文件头自称是「napi 边界运行时漂移捕获器」，但 [session-handle.ts](file:///G:/kimirust/packages/kimi-agent/session-handle.ts#L14) 实际只接入了 `authTokenRequestSchema/authTokenResponseSchema` 两个，其余 13 个值 + 6 个类型从未被 parse 调用 —— **意图与现状矛盾，建议「接线」而非「删除」，见裁决点 1**。
- `packages/protocol` 的 ~150 个零引用类型 + 13 个 schema：REST/WS 契约面，消费者是外部宿主与 prebuilt 前端，**不计入可删**。
- `packages/node-sdk/src/config-local/schema.ts` 一批 `*Schema`：经同文件内组合 schema 间接使用的已排除，剩余为零引用但属于配置契约面，删除前需确认对外兼容。

## 六、波次划分汇总

### 波次 1 · 可立即安全删除（与 `git status` 中活跃改动零交集）

- apps/vscode/webview-ui/src/components/ui/ ×12 + components/index.ts
- apps/vis/web/src/components/shared/Drawer.tsx
- packages/telemetry/src/context.ts
- packages/kimi-agent/Cargo.toml 3 个依赖（dialoguer / tower-lsp / lsp-types）
- 各非活跃包内的零散死导出（附录 A 中 kosong/minidb/pi-tui 等条目，逐条确认后随批删除）

验证要求（第二阶段执行）：每批删除后跑 `bun run check:all` + `cargo clippy --all-targets` + `bun run typecheck`。

### 波次 2 · 需等待（位于当前在跑批次的目录）

- apps/kimi-code：workspace-preheat.ts、constant/index.ts、tui/components/index.ts、coding-plan-config.ts 及附录 A 中 apps/kimi-code 的 14 条值导出
- packages/kimi-agent：napi-debug.mjs、wire-schema.ts 未接线 schema（先裁决）
- packages/node-sdk：v2/global-mcp.ts 及附录 A 中 21 条值导出
- packages/protocol：rest/session.ts 等文件内死导出（该文件在未提交 diff 中）

### 波次 3 · 不可删

- Rust 15 处 `#[allow(dead_code)]`（§2，全部为 parity/契约/Drop 语义）
- napi 生成物（index.js、index.native.cjs、index.d.ts、target/napi-generated.d.ts）
- workflow builtin 9 个 js、normify cli/plugin-bin、minidb worker、examples/bench 入口
- kosong/oauth 公开导出面、packages/protocol 契约面（待裁决）
- tree-sitter-bash 整包（接线中）
- kimi-web 整包（特例，待裁决）

## 七、需要你裁决的点

1. **wire-schema.ts 的 19 个未接线 schema**：文件注释声明的「运行时漂移捕获」意图未落地。建议补齐接线（在 session-handle 的 napi 边界统一 parse），而不是删除。你选哪条？
2. **kosong / oauth 的公开导出零消费**（inspection、request-trace、message-helpers、errors/*、oauth/device）：包虽 `private: true` 但 exports 面是刻意对齐上游的。保留还是收缩？
3. **kimi-web 未用 Vue 组件（13 个）+ 31 条零散导出**：kimi-web 已排除出 workspace 且 dist-web 不可重建。源码是否继续作为参考准绳保留？
4. **vscode webview 的 12 个 shadcn 组件**：零引用，但属于「组件库」性质。直接删，还是保留备用？（我的建议：删，shadcn 随时可按需重新生成）
5. **minidb/bench ×17、node-sdk/examples ×17、oauth/examples ×1**：手动入口，无门禁引用。保留为开发工具，还是精简？
6. **Cargo 依赖 tower-lsp / lsp-types**：是否为尚未移植的 LSP 能力预留？若是则保留并在 ROADMAP 标注。

## 附录 A · 零引用值导出（102 条）

判定依据：符号名全仓单词出现 ≤1（仅定义处），且定义文件内无第二次引用；抽查精度 10/10。

```
apps\kimi-code\src\cli\sub\web\legacy-kill.ts :: LEGACY_SERVER_MAX_VERSION
apps\kimi-code\src\cli\sub\web\shared.ts :: DEFAULT_SERVER_ORIGIN
apps\kimi-code\src\cli\workspace-preheat.ts :: preheatWorkspaceIndex
apps\kimi-code\src\constant\app.ts :: PROMPT_CLEANUP_TIMEOUT_MS
apps\kimi-code\src\constant\app.ts :: WEB_USER_AGENT_SUFFIX
apps\kimi-code\src\native\native-assets.ts :: nativeAssetCacheExists
apps\kimi-code\src\tui\components\dialogs\coding-plan-config.ts :: CodingPlanConfigComponent
apps\kimi-code\src\tui\components\dialogs\model-selector.ts :: createModelChoiceOptions
apps\kimi-code\src\tui\components\panes\activity-pane.ts :: formatActivitySpinnerTip
apps\kimi-code\src\tui\constant\feedback.ts :: feedbackHttpErrorMessage
apps\kimi-code\src\tui\constant\feedback.ts :: getFeedbackStatusUploading
apps\kimi-code\src\tui\constant\rendering.ts :: NOTIFY_PANEL_PAGE_LINES
apps\kimi-code\src\tui\theme\index.ts :: getColorPaletteSync
apps\kimi-code\src\tui\theme\index.ts :: isThemeName
apps\kimi-code\src\utils\client-configs.ts :: refreshClientConfigInBackground
apps\kimi-inspect\src\compat\v2.ts :: IAgentPromptService
apps\kimi-inspect\src\compat\v2.ts :: ISessionApprovalService
apps\kimi-inspect\src\compat\v2.ts :: ISessionQuestionService
apps\kimi-inspect\src\snapshots\api.ts :: fetchWorkspaceSnapshots
apps\kimi-web\src\api\daemon\agentEventProjector.ts :: isRawAgentCoreEvent
apps\kimi-web\src\api\daemon\wire.ts :: WIRE_EVENT_TYPES
apps\kimi-web\src\composables\useFileTreeBrowser.ts :: entryPassesFilter
apps\kimi-web\src\composables\useFileTreeBrowser.ts :: isFilterActive
apps\kimi-web\src\lib\cronHumanize.ts :: collapsePrompt
apps\kimi-web\src\lib\trajectory\timeline.ts :: formatTimelineOffset
apps\kimi-web\src\lib\trajectory\virtualRows.ts :: TRAJECTORY_COLLAPSED_SUMMARY_HEIGHT
apps\kimi-web\src\lib\trajectory\virtualRows.ts :: groupTrajectoryVirtualRows
apps\vis\server\src\lib\context-memory.ts :: isValidUndoCount
apps\vis\server\src\lib\v2-wire.ts :: migrateWireRecords
apps\vis\web\src\components\shared\Drawer.tsx :: Drawer
apps\vscode\shared\errors.ts :: classifyError
apps\vscode\src\i18n\index.ts :: refreshLocale
apps\vscode\webview-ui\src\services\config.ts :: FILE_PICKER_CONFIG
packages\kimi-agent\wire-schema.ts :: llmChatRequestSchema
packages\kimi-agent\wire-schema.ts :: permissionCheckRequestSchema
packages\kimi-agent\wire-schema.ts :: runTurnParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionBackgroundTaskOutputParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionBackgroundTaskStopParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionBtwPromptParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionCancelParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionEnqueueTurnParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionGenerateTitleParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionHistoryParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionIdParamsSchema
packages\kimi-agent\wire-schema.ts :: sessionTurnOutcomeParamsSchema
packages\kimi-agent\wire-schema.ts :: toolExecuteRequestSchema
packages\kosong\src\errors.ts :: PROVIDER_API_ERROR_CODE
packages\kosong\src\errors\errors.ts :: ExpectedError
packages\kosong\src\errors\errors.ts :: isError2
packages\kosong\src\errors\errors.ts :: unwrapErrorCause
packages\kosong\src\errors\unexpectedError.ts :: resetUnexpectedErrorHandler
packages\kosong\src\errors\unexpectedError.ts :: safelyCallListener
packages\minidb\src\codec.ts :: parseBuffer
packages\minidb\src\codec.ts :: scanFrameRefsFile
packages\minidb\src\crc32.ts :: _private
packages\minidb\src\gen-codec.ts :: readTextDictionaryImage
packages\minidb\src\worker\text-build-core.ts :: isTextBuildSegmentFile
packages\minidb\src\worker\text-build-core.ts :: readBaseDocsImage
packages\minidb\test\e2e\helpers\prng.ts :: seedFromString
packages\minidb\test\helpers.ts :: failCalls
packages\node-sdk\src\config-local\path.ts :: ensureKimiHome
packages\node-sdk\src\config-local\resolve.ts :: parseFloatEnv
packages\node-sdk\src\config-local\resolve.ts :: resolveConfigValue
packages\node-sdk\src\logging.ts :: resolveSessionLogPath
packages\node-sdk\src\media\image-compress.ts :: READ_IMAGE_BYTE_BUDGET
packages\node-sdk\src\media\image-format.ts :: decodeBase64Prefix
packages\node-sdk\src\media\mediaRef.ts :: matchSingleMediaPathTag
packages\node-sdk\src\media\mediaRef.ts :: mediaExtensionForMime
packages\node-sdk\src\media\mediaRef.ts :: mediaKindForMime
packages\node-sdk\src\media\mediaRef.ts :: mediaKindForPath
packages\node-sdk\src\media\mediaRef.ts :: sessionMediaFilePath
packages\node-sdk\src\native\sdk-client\helpers.ts :: truncateHistoryByTurns
packages\node-sdk\src\proxy.ts :: isProxyConfigured
packages\node-sdk\src\proxy.ts :: proxyEnvForChild
packages\node-sdk\src\proxy.ts :: reconcileChildNoProxy
packages\node-sdk\src\rpc.ts :: ClientAPI
packages\node-sdk\src\v2\config-mapper.ts :: diagnosticsToConfigDiagnostics
packages\node-sdk\src\v2\config-mapper.ts :: resolvedConfigToKimiConfig
packages\node-sdk\src\v2\global-mcp.ts :: normalizeServerName
packages\node-sdk\src\v2\global-mcp.ts :: parseInlineMcpServer
packages\node-sdk\src\v2\global-mcp.ts :: parseReconnectMcpServerConfig
packages\normify\src\engine\diag.ts :: emptyResult
packages\normify\src\engine\diag.ts :: failResult
packages\oauth\src\managed-kimi-code.ts :: provisionManagedKimiCodeConfigAfterLogin
packages\pi-tui\src\utils.ts :: isPunctuationChar
packages\protocol\src\events.ts :: goalBudgetLimitsSchema
packages\protocol\src\events.ts :: goalToolResultSchema
packages\protocol\src\events.ts :: mcpOAuthAuthorizationUrlUpdateDataSchema
packages\protocol\src\rest\guiStore.ts :: guiStoreGetItemQuerySchema
packages\protocol\src\rest\guiStore.ts :: guiStoreRemoveItemBodySchema
packages\protocol\src\rest\guiStore.ts :: guiStoreSetItemBodySchema
packages\protocol\src\rest\session.ts :: deleteSessionResponseSchema
packages\protocol\src\rest\session.ts :: updateSessionMetaRequestSchema
packages\protocol\src\rest\session.ts :: updateSessionMetaResponseSchema
packages\protocol\src\wire.ts :: MCP_OAUTH_META_SUFFIX
packages\protocol\src\wire.ts :: MCP_OAUTH_TOKENS_SUFFIX
packages\protocol\src\ws-control.ts :: WS_PROTOCOL_VERSION
packages\protocol\src\ws-control.ts :: helloAckPayloadSchema
packages\transcript\src\contract\mediaRef.ts :: matchMediaPathTagText
packages\transcript\src\contract\mediaRef.ts :: parseDaemonFileRefFileId
packages\transcript\src\contract\schema.ts :: transcriptOpBatchSchema
packages\transcript\src\model\item.ts :: KNOWN_MARKERS
```

## 附录 B · 零引用类型导出（195 条，低置信）

类型常被跨包以 `z.infer`、声明合并、结构化匹配等方式隐式消费，且 packages/protocol 的类型即 REST/WS 对外契约面。**默认不建议删，仅作审计参考。**

```
apps\kimi-code\src\tui\theme\custom-theme-loader.ts :: CustomThemeDefinition
apps\kimi-code\src\tui\types.ts :: ProgressSpinnerHandle
apps\kimi-code\src\tui\utils\background-task-status.ts :: BackgroundTaskTranscriptPhase
apps\kimi-code\src\utils\clipboard\clipboard-common.ts :: RunCommandAsync
apps\kimi-web\src\api\daemon\wire.ts :: WireClientControl
apps\kimi-web\src\composables\client\useWorkspaceState.ts :: UseWorkspaceState
apps\kimi-web\src\types.ts :: WorkspaceScope
apps\vis\server\src\lib\v2-wire.ts :: AgentTaskKind
apps\vis\server\src\lib\v2-wire.ts :: TurnStepCompleted
packages\kimi-agent\wire-schema.ts :: SessionBtwPromptWire
packages\kimi-agent\wire-schema.ts :: SessionMessageWire
packages\kimi-agent\wire-schema.ts :: SessionStatusWire
packages\kimi-agent\wire-schema.ts :: SessionTurnOutcomeWire
packages\kimi-agent\wire-schema.ts :: TelemetryEventWire
packages\kimi-agent\wire-schema.ts :: TurnEventWire
packages\kosong\src\provider.ts :: ToolCallIdPolicy
packages\kosong\src\request-trace.ts :: LLMRequestTrace
packages\minidb\src\stats.ts :: MiniDbStats
packages\node-sdk\src\config-helpers.ts :: HookEventType
packages\node-sdk\src\config-local\schema.ts :: McpRemoteServerConfig
packages\node-sdk\src\config-local\schema.ts :: McpServerStdioConfig
packages\node-sdk\src\config-local\schema.ts :: ShellConfig
packages\node-sdk\src\media\mediaRef.ts :: AudioMimeType
packages\node-sdk\src\media\mediaRef.ts :: ImageMimeType
packages\node-sdk\src\media\mediaRef.ts :: VideoMimeType
packages\node-sdk\src\types.ts :: TurnEngine
packages\protocol\src\display.ts :: ToolResultDisplay
packages\protocol\src\fs.ts :: FsChangeAction
packages\protocol\src\fs.ts :: FsChangeKind
packages\protocol\src\fs.ts :: FsGitStatus
packages\protocol\src\message.ts :: MessageRole
packages\protocol\src\message.ts :: TextContent
packages\protocol\src\message.ts :: ThinkingContent
packages\protocol\src\message.ts :: ToolResultContent
packages\protocol\src\message.ts :: ToolUseContent
packages\protocol\src\modelCatalog.ts :: ProviderCatalogStatus
packages\protocol\src\pagination.ts :: PageResponse
packages\protocol\src\rest\approval.ts :: ApprovalAlreadyResolvedData
packages\protocol\src\rest\approval.ts :: ApprovalResolveRequest
packages\protocol\src\rest\approval.ts :: ListPendingApprovalsQuery
packages\protocol\src\rest\approval.ts :: ListPendingApprovalsResponse
packages\protocol\src\rest\auth.ts :: ManagedProviderStatus
packages\protocol\src\rest\auth.ts :: ManagedProviderSummary
packages\protocol\src\rest\config.ts :: PatchConfigRequest
packages\protocol\src\rest\config.ts :: ProviderConfigResponse
packages\protocol\src\rest\file.ts :: DeleteFileParam
packages\protocol\src\rest\file.ts :: GetFileParam
packages\protocol\src\rest\fs.ts :: FsDownloadParams
packages\protocol\src\rest\fs.ts :: FsListManyPartialError
packages\protocol\src\rest\fs.ts :: FsListSort
packages\protocol\src\rest\fs.ts :: FsMkdirResponse
packages\protocol\src\rest\fs.ts :: FsOpenInAppId
packages\protocol\src\rest\fs.ts :: FsOpenInRequest
packages\protocol\src\rest\fs.ts :: FsOpenInResponse
packages\protocol\src\rest\fs.ts :: FsPullRequest
packages\protocol\src\rest\fs.ts :: FsReadEncoding
packages\protocol\src\rest\fs.ts :: FsRootSuggestRequest
packages\protocol\src\rest\fs.ts :: FsStatResponse
packages\protocol\src\rest\fs.ts :: FsSuggestRequest
packages\protocol\src\rest\fs.ts :: FsSuggestResponse
packages\protocol\src\rest\fsBrowse.ts :: FsBrowseQuery
packages\protocol\src\rest\guiStore.ts :: GuiStoreGetItemResponse
packages\protocol\src\rest\guiStore.ts :: GuiStoreLengthResponse
packages\protocol\src\rest\message.ts :: GetMessageResponse
packages\protocol\src\rest\message.ts :: ListMessagesQuery
packages\protocol\src\rest\message.ts :: ListMessagesResponse
packages\protocol\src\rest\meta.ts :: MetaCapabilities
packages\protocol\src\rest\modelCatalog.ts :: GetProviderResponse
packages\protocol\src\rest\modelCatalog.ts :: ImportCatalogProviderResponse
packages\protocol\src\rest\modelCatalog.ts :: ListModelsResponse
packages\protocol\src\rest\modelCatalog.ts :: ListProvidersResponse
packages\protocol\src\rest\modelCatalog.ts :: RefreshOAuthProviderModelsResponse
packages\protocol\src\rest\modelCatalog.ts :: RefreshProviderModelsResponse
packages\protocol\src\rest\modelCatalog.ts :: SetDefaultModelResponse
packages\protocol\src\rest\oauth.ts :: OAuthFlowSnapshot
packages\protocol\src\rest\oauth.ts :: OAuthFlowStartAuthenticated
packages\protocol\src\rest\oauth.ts :: OAuthFlowStartPending
packages\protocol\src\rest\oauth.ts :: OAuthLoginCancelResponse
packages\protocol\src\rest\oauth.ts :: OAuthLoginQuery
packages\protocol\src\rest\oauth.ts :: OAuthLoginStartRequest
packages\protocol\src\rest\oauth.ts :: OAuthLogoutRequest
packages\protocol\src\rest\oauth.ts :: OAuthLogoutResponse
packages\protocol\src\rest\prompt.ts :: PromptAbortResponse
packages\protocol\src\rest\prompt.ts :: PromptAbortedEventPayload
packages\protocol\src\rest\prompt.ts :: PromptCompletedEventPayload
packages\protocol\src\rest\prompt.ts :: PromptListResponse
packages\protocol\src\rest\prompt.ts :: PromptPermissionMode
packages\protocol\src\rest\prompt.ts :: PromptStatus
packages\protocol\src\rest\prompt.ts :: PromptSteerRequest
packages\protocol\src\rest\prompt.ts :: PromptSteerResult
packages\protocol\src\rest\prompt.ts :: PromptSteeredEventPayload
packages\protocol\src\rest\prompt.ts :: PromptThinking
packages\protocol\src\rest\question.ts :: ListPendingQuestionsQuery
packages\protocol\src\rest\question.ts :: ListPendingQuestionsResponse
packages\protocol\src\rest\question.ts :: QuestionAlreadyResolvedData
packages\protocol\src\rest\question.ts :: QuestionDismissResult
packages\protocol\src\rest\question.ts :: QuestionResolveRequest
packages\protocol\src\rest\remoteControl.ts :: RemoteControlState
packages\protocol\src\rest\remoteControl.ts :: SetRemoteControlRequest
packages\protocol\src\rest\session.ts :: CompactSessionRequest
packages\protocol\src\rest\session.ts :: CompactSessionResponse
packages\protocol\src\rest\session.ts :: CreateSessionChildRequest
packages\protocol\src\rest\session.ts :: CreateSessionChildResponse
packages\protocol\src\rest\session.ts :: CreateSessionRequest
packages\protocol\src\rest\session.ts :: CreateSessionResponse
packages\protocol\src\rest\session.ts :: ExportSessionParams
packages\protocol\src\rest\session.ts :: ExportSessionRequest
packages\protocol\src\rest\session.ts :: ForkSessionRequest
packages\protocol\src\rest\session.ts :: ForkSessionResponse
packages\protocol\src\rest\session.ts :: GetSessionGoalResponse
packages\protocol\src\rest\session.ts :: GetSessionProfileResponse
packages\protocol\src\rest\session.ts :: GetSessionResponse
packages\protocol\src\rest\session.ts :: ListSessionChildrenQuery
packages\protocol\src\rest\session.ts :: ListSessionChildrenResponse
packages\protocol\src\rest\session.ts :: ListSessionsQuery
packages\protocol\src\rest\session.ts :: RestoreSessionResponse
packages\protocol\src\rest\session.ts :: SessionAbortResponse
packages\protocol\src\rest\session.ts :: SessionWarning
packages\protocol\src\rest\session.ts :: SessionWarningsResponse
packages\protocol\src\rest\session.ts :: StartBtwSessionResponse
packages\protocol\src\rest\session.ts :: UndoSessionRequest
packages\protocol\src\rest\session.ts :: UndoSessionResponse
packages\protocol\src\rest\session.ts :: UpdateSessionMetaRequest
packages\protocol\src\rest\session.ts :: UpdateSessionMetaResponse
packages\protocol\src\rest\session.ts :: UpdateSessionRequest
packages\protocol\src\rest\session.ts :: UpdateSessionResponse
packages\protocol\src\rest\skill.ts :: ActivateSkillAttachment
packages\protocol\src\rest\skill.ts :: ActivateSkillRequest
packages\protocol\src\rest\skill.ts :: ActivateSkillResult
packages\protocol\src\rest\skill.ts :: ListSkillsResponse
packages\protocol\src\rest\snapshot.ts :: InFlightToolCall
packages\protocol\src\rest\snapshot.ts :: InFlightTurn
packages\protocol\src\rest\snapshot.ts :: SessionSnapshotResponse
packages\protocol\src\rest\snapshot.ts :: SnapshotSubagent
packages\protocol\src\rest\task.ts :: CancelTaskResult
packages\protocol\src\rest\task.ts :: DetachTaskResult
packages\protocol\src\rest\task.ts :: GetTaskQuery
packages\protocol\src\rest\task.ts :: GetTaskResponse
packages\protocol\src\rest\task.ts :: ListTasksQuery
packages\protocol\src\rest\task.ts :: ListTasksResponse
packages\protocol\src\rest\task.ts :: TaskAlreadyFinishedData
packages\protocol\src\rest\terminal.ts :: CloseTerminalResponse
packages\protocol\src\rest\terminal.ts :: CreateTerminalRequest
packages\protocol\src\rest\terminal.ts :: GetTerminalResponse
packages\protocol\src\rest\terminal.ts :: ListTerminalsResponse
packages\protocol\src\rest\terminal.ts :: TerminalStatus
packages\protocol\src\rest\tool.ts :: ListMcpServersResponse
packages\protocol\src\rest\tool.ts :: ListToolsQuery
packages\protocol\src\rest\tool.ts :: ListToolsResponse
packages\protocol\src\rest\tool.ts :: RestartMcpServerResult
packages\protocol\src\rest\workspace.ts :: CreateWorkspaceRequest
packages\protocol\src\rest\workspace.ts :: CreateWorkspaceResponse
packages\protocol\src\rest\workspace.ts :: DeleteWorkspaceResponse
packages\protocol\src\rest\workspace.ts :: UpdateWorkspaceRequest
packages\protocol\src\rest\workspace.ts :: UpdateWorkspaceResponse
packages\protocol\src\rest\workspace.ts :: WorkspaceIdParam
packages\protocol\src\session.ts :: PermissionRule
packages\protocol\src\session.ts :: SessionAgentConfig
packages\protocol\src\session.ts :: SessionAgentConfigPartial
packages\protocol\src\session.ts :: SessionChildCreate
packages\protocol\src\task.ts :: BackgroundTaskKind
packages\protocol\src\tool.ts :: McpServerTransport
packages\protocol\src\tool.ts :: ToolSource
packages\protocol\src\wire.ts :: McpOAuthStore
packages\protocol\src\wire.ts :: McpOAuthStoreMeta
packages\protocol\src\workspace.ts :: WorkspaceId
packages\protocol\src\ws-control.ts :: AbortMessage
packages\protocol\src\ws-control.ts :: AgentFilter
packages\protocol\src\ws-control.ts :: ClientControlMessage
packages\protocol\src\ws-control.ts :: ClientHelloMessage
packages\protocol\src\ws-control.ts :: CursorsBySession
packages\protocol\src\ws-control.ts :: PingMessage
packages\protocol\src\ws-control.ts :: PongMessage
packages\protocol\src\ws-control.ts :: ResyncRequiredMessage
packages\protocol\src\ws-control.ts :: ServerHelloMessage
packages\protocol\src\ws-control.ts :: ServerSystemMessage
packages\protocol\src\ws-control.ts :: SubscribeMessage
packages\protocol\src\ws-control.ts :: SubscribeV2Message
packages\protocol\src\ws-control.ts :: TerminalAttachMessage
packages\protocol\src\ws-control.ts :: TerminalCloseMessage
packages\protocol\src\ws-control.ts :: TerminalDetachMessage
packages\protocol\src\ws-control.ts :: TerminalExitMessage
packages\protocol\src\ws-control.ts :: TerminalInputMessage
packages\protocol\src\ws-control.ts :: TerminalOutputMessage
packages\protocol\src\ws-control.ts :: TerminalResizeMessage
packages\protocol\src\ws-control.ts :: UnsubscribeMessage
packages\protocol\src\ws-control.ts :: UnsubscribeV2Message
packages\protocol\src\ws-control.ts :: WsErrorMessage
packages\transcript\src\contract\events.ts :: TranscriptEvent
packages\transcript\src\contract\events.ts :: TranscriptEventType
packages\transcript\src\contract\schema.ts :: TranscriptSubscribeV2Payload
packages\transcript\src\model\ids.ts :: ItemId
packages\transcript\src\view\registry.ts :: InputViewContext
packages\transcript\src\view\registry.ts :: MarkerViewContext
packages\transcript\src\view\registry.ts :: ToolViewContext
```
