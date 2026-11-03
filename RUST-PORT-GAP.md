# Rust 移植缺口报告（RUST-PORT-GAP）

> 快照时间：2026-11-02（Asia/Shanghai）。分支 `main`，HEAD `b2c89a8fe`。
> 工作区含前两批未提交改动（`[mcp]` 全局默认值 napi 通道、`napi_bindings.rs` → 9 文件拆分、
> subagent/todo 提醒缺陷项、`check:all` 聚合门禁），本报告全部基于**当前工作区**读取，
> 未修改、未回滚、未格式化任何文件，未产生提交。
> 行号为快照时点物理行；另一会话可能并行改动本仓库，引用前请按符号复核。

---

## 0. 结论摘要

1. **主移植已完成，剩余是「长尾 + 地基 + 裁决」三类。** 四大退役包（agent-core-v2 / kap-server /
   klient / acp-server）的核心面已由 `packages/kimi-agent`（303 个 `.rs`）替代，67 条 REST、
   27 个 WS 事件、12 个 WS 控制帧、89 个工具名、105 个 napi 导出全部在 parity 门禁下对齐。
   剩余缺口 = 57 条已登记的 upstream tracked delta + 10 个整包/整层未移植项 + 约 15 处
   已迁移但行为不一致点。
2. **最痛的不是缺功能，是三处「双源」**：配置（TS `config-local/schema.ts` 584 行 vs Rust
   `config/mod.rs` 1,800 行，每加一个配置面要改四层）、LLM 抽象（`packages/kosong` 仍被
   vis-server / oauth / node-sdk 消费）、认证与模型目录（`packages/oauth` 29 文件，上游
   `/models` schema 等行为的落点在宿主 TS 侧，Rust 够不着）。
3. ~~**有一个「零消费者死资产」**~~：`packages/tree-sitter-bash`（7 文件 4,806 行递归下降
   bash 解析器，带对官方 wasm 的差分测试）~~**没有任何生产代码 import 它**~~ **（2026-11-02 起
   有第一个生产消费者：`node-sdk` 经 `host/parse_bash` 回调为 AGENTS.md 操作数目录提醒供树）**；
   Rust 引擎的危险命令检测（`native/permission_engine/dangerous_command.rs`）仍在保守退化
   （权限放宽单独成批再评），AGENTS.md 操作数目录一半已解。
4. **57 条 tracked delta 里，真正高用户影响的是约 12 条**（跨进程会话锁、压缩丢动态工具
   上下文、媒体状态不跨轮、MCP 公告先于发现、子代理 profile allowlist 断链、统一 /models
   schema、global 区遥测端点等）；其余为体验打磨、新产品面（remote control 设备管理、评分
   规则、多根工作区）或待裁决政策项（MCP 自动重连、Plan 模式交还用户、prompt 成批改写）。
5. **台账可信度**：本轮对台账/allowlist 的关键声明做了 12 处抽样回源核实，**11 处属实、
   1 处过期**（§1 矩阵称 external hooks 只覆盖 8/20 事件——实际 §10.27 与 §42/§43 已补齐，
   20 种事件全部有触发点；矩阵行未回填）。凡本轮未核实的条目在文中一律标
   **[台账待核]** 并给出验证方法。

**审计手段与覆盖**（每条结论的来源见 §6）：`bun run check:all` 16 门禁端到端 EXIT=0（本轮
实跑）；`check:parity` / `check:upstream-v2-delta` / `check:architecture` /
`check:rust-module-deps` 单独复跑并解读输出语义；`upstream-v2-delta-allowlist.json`
57 条 tracked 全量提取；ROADMAP §1 矩阵与 §6.46 分组精读；TS 侧 11 个包逐一清点消费方。
**覆盖不到**：运行时行为（本报告全部结论为静态阅读 + 门禁输出，未跑测试与基准）；
`apps/vis`、`apps/vscode`、`plugins/`、`scripts/prompt-optimizer` 未深入。

---

## 1. 现状基线（数字与门禁事实）

| 维度 | 数字 | 证据（本轮实跑/实读） |
|---|---|---|
| Rust 引擎 | 303 个 `.rs`，39 个顶层模块，253 对模块依赖与基线一致 | `check:rust-module-deps` 输出 |
| 接口 parity | REST 67 / WS server 27（web-only no-op 5）/ WS ctl 12 / tools 89 / napi 105 / config 31 键 / nllm 19↔17 / shell 顺序 pwsh>powershell>bash>cmd | `check:parity` 输出，EXIT=0 |
| 上游 delta | 99 条全 triaged：ported=18 / tracked=57 / not-applicable=24；ref `upstream/main @ a2afdbc7d`（2026-10-30） | `check:upstream-v2-delta` 输出 |
| 架构 | 17 模块，1 条豁免警告（i18n-runtime 对 kimi-agent 的运行期软依赖） | `check:architecture` 输出 |
| 聚合门禁 | `check:all`（16 个门禁）EXIT=0 | 本轮实跑 |
| model 字段 | TS `ModelAliasBaseSchema` 22 键 + `overrides` = 23 个概念；Rust `ModelAliasConfig` 的 37 是 serde rename+alias 双计数，归一化后同为 23，**无差集** | 本轮脚本实算（`.tmp/model-fields-diff.mjs`） |
| TS 侧规模 | node-sdk 50 文件 18,970 行；kimi-code 362 文件 67,252 行；minidb 61/18,633；pi-tui 43/17,003；protocol 73/10,010；normify 23/8,260；oauth 29/6,987；tree-sitter-bash 7/4,806；transcript 25/3,562；kosong 21/2,996；kaos 12/2,979；telemetry 11/1,191 | 本轮实测 |

**parity 门禁的盲区（本报告的覆盖即补这些）**：它只比对「界面」（REST 路径、事件名、工具名、
napi 函数名、配置键、shell 顺序），**不比对行为语义**——默认值、错误处理、时序、状态生命周期
全在它视野之外。行为差异的唯一登记处是 upstream-delta allowlist 的 note 与 ROADMAP §6.x，
二者都是人工台账，这正是本报告存在的原因。

---

## 2. 缺口清单 A：整包 / 整层未移植（TS-only）

排序 = 用户可感知影响 × 移植成本（高影响低成本在前）。「现在由谁承担」指该能力今天的实际
执行者。

### A1. `packages/tree-sitter-bash` —— bash 语法树（死资产 vs 两处饥渴消费方）

- **缺什么**：Rust 侧没有 bash 解析器。两处已知退化都源于此：① ~~AGENTS.md 提醒的
  「Bash 操作数目录」分支~~ **① 已解（2026-11-02，走 napi 宿主回调路：`host/parse_bash`，
  宿主侧用 tree-sitter-bash + 照 v2 `bashTargets.ts` 的 walker；回调缺席/失败/无 JS 宿主
  一律落回 cwd-only，即 v2 自己的 `!parsed.ok` 降级臂）**；② 危险命令检测
  `native/permission_engine/dangerous_command.rs:11-18` 自陈「无 tree-sitter 语义、朴素分词器
  保守退化」——**本轮裁定保持不动，权限放宽单独成批再评**。
- **现在由谁承担**：① 的 walker 在 `node-sdk/src/native/sdk-client/bash-target-dirs.ts`
  （tree-sitter-bash 的第一个生产消费者）；② 仍是朴素分词器。
- **涉及文件**：`packages/tree-sitter-bash/src/parser.ts`（~2,000 行核心）等 7 文件；
  落点在 `packages/kimi-agent/src/native/permission_engine/dangerous_command.rs` 与
  `src/injection/mod.rs`（`StepAccess` 的 Bash 分支）。
- **前置依赖**：无（自研解析器，不依赖上游 v2 的 bash-parser 包——后者已随退役删除）。
- **成本评估**：高（移植解析器 + 差分契约数千行），但有另一条低成本路：napi 宿主回调让
  TS 侧现成解析器算目录集合（ROADMAP §1 板块 5 已列出这两条路）。
- **证据**：本轮 grep（零生产 import）；`dangerous_command.rs:11-18` 文件头；ROADMAP §1 板块 5。

### A2. `packages/kosong` —— TS LLM 抽象层（双源裁决项）

- **缺什么**：Rust `src/llm/` 已是主链路唯一执行者；kosong 的 provider wire 层已退役。
  残余活体：`catalog.ts`（22.8KB，模型目录）、`tokens.ts`（`estimateTokensForMessages`，
  node-sdk `sdk-rpc-client-native.ts:34` 在用）、`errors.ts`（28.3KB，错误分类）、
  `capability.ts`、`message.ts` 类型层。
- **现在由谁承担**：TS kosong（vis-server 运行时依赖、oauth 构建期依赖、node-sdk 6 处
  import——见 `catalog.ts:13`、`kimi-code-model-provider.ts:15-16`、`model-alias.ts:10`、
  `error-protocol/serialize.ts:8`、`events.ts:1`、`types.ts:3,746`）。
- **涉及文件**：`packages/kosong/src` 21 文件 2,996 行；消费方见上。
- **前置依赖**：vis-server 的终态裁决（走引擎 RPC 还是保留独立 LLM 包）。
- **判断**：中期「全 Rust」下必须裁终态（DEVELOPMENT-OPTIMIZATION S5）。token 估算与错误
  分类是 Rust 已有对应物（`llm/error.rs`、`turn_loop` 的 token 计数）的候选搬迁面。
- **证据**：architecture.json:83-92；本轮 import grep。

### A3. `packages/oauth` —— 托管认证与模型目录（宿主层，上游行为落点）

- **缺什么**：不是「移植到 Rust」而是「上游新行为持续落在 TS 宿主」。两条 tracked delta
  的落点在此：统一 `/models` schema（`9f0693becb`：managed 路径仍读扁平 `capabilities`，
  **本轮已核实** `managed-kimi-code.ts:432-433,553-568` 读扁平数组；`limit.context` /
  `modalities` 解析只存在于 `custom-registry.ts:177-216`，不管 managed 路径）；
  模型迁移元数据（`0327e8e9a8`，宿主无任何 `migration` 字段，grep 零命中）。
  另：global 区遥测端点缺失（`da4e2f57a3`，**本轮已核实** `region.ts:70-76` 的
  `global` profile 无 `telemetryEndpoint`，`:67` 仅 mainland-cn 有）。
- **现在由谁承担**：TS oauth 包（29 文件 6,987 行；consumer：node-sdk `auth.ts:20` /
  `kimi-code-model-provider.ts:11` / `oauth-error.ts:5` / sdk-rpc-client-native、
  kimi-code、vscode）。
- **涉及文件**：`packages/oauth/src/managed-kimi-code.ts`、`refreshProviderModels.ts`、
  `region.ts`；Rust 侧消费方 `server/oauth.rs`（43KB，服务端 OAuth 路由已在 Rust）。
- **前置依赖**：无（纯宿主侧补齐）。
- **证据**：本轮实读；allowlist note。

### A4. `packages/minidb` —— 嵌入式读模型（未接线层）

- **缺什么**：61 文件 18,633 行的 WAL + 快照 + trigram 全文索引 + 复合索引实现，引擎侧
  **零引用**（`minidb`/`IQueryStore`/`read_model` 在 `kimi-agent/src` 零命中）；唯一引用是
  `apps/kimi-code/src/native/minidb-worker.ts` 一条 smoke 路径 + kimi-code package.json 依赖。
  上游它同时是 `ISessionIndex`、projector、mirror、dirty-journal 与全局搜索 worker 的基底
  （ROADMAP §1 板块 10）。
- **现在由谁承担**：Rust `session/sqlite_store.rs` + `storage/state_store.rs` 承担了写路径
  持久化；**读模型/全文检索/跨会话索引无人承担**（「fork 是否需要会话全文检索」在台账里
  是未裁决项）。
- **涉及文件**：`packages/minidb/src`；若裁为「需要」，落点是 Rust sqlite 侧建读模型而非
  复活 TS minidb。
- **前置依赖**：先裁决「会话全文检索是否需要」。
- **证据**：ROADMAP §1 板块 10（2026-10-02 补注）；本轮 grep 复核零引用属实。

### A5. `packages/kaos` —— 执行环境抽象（类型残留，与上游远程环境冲突）

- **缺什么**：运行时 unwired——node-sdk 只 import `type Kaos`（`kimi-harness.ts:1,152-221`、
  `rpc.ts:10`、`types.ts:1`），`createSessionWithKaos` 的 kaos 参数穿过 API 面但仓内无
  实际执行消费者；12 文件 2,979 行（含 `ssh.ts` 35.5KB、`local.ts` 31.5KB）实为死重。
  上游远程执行环境（SSH/Docker，`5f3ab5fac8`）与本包冲突，台账标「待裁决」。
- **现在由谁承担**：本地执行 = Rust `native/bash_spawn.rs`；远程执行 = 无人。
- **涉及文件**：`packages/kaos/src`；若裁远程环境，落点在 Rust `native/`。
- **前置依赖**：远程环境政策裁决（§4 阶段 0）。
- **证据**：本轮 grep（apps 零 import；node-sdk 仅 type）。

### A6. `packages/transcript` —— 转录渲染数据层

- **缺什么**：25 文件 3,562 行，consumer 是 `packages/protocol`（依赖）与
  `apps/kimi-inspect`。Rust 侧有 `events/` + `native/event_store/` + `server/transcript/`，
  但 **TS transcript 是 kimi-inspect 与 protocol 类型的同构渲染层**，不是引擎运行时缺口。
  真正的引擎侧缺口是 wire 记录无版本概念（见 B 表 #12）。
- **判断**：按「app 层保留 TS」原则，本包**不该移植**；它出现在这里只为说明边界。
- **证据**：architecture.json:111-118；本轮清点。

### A7. 协议/共享层（`protocol` / `telemetry` / `i18n-*` / `pi-tui` / `normify`）

- 均为**按架构应留 TS** 的包：protocol 是 wire schema 的权威源（Rust 侧经
  `ws-event-contract.json` / `tool-name-contract.json` / `napi-contract.d.ts` + parity 门禁
  镜像）；telemetry/i18n/pi-tui 属 shared/app 层；normify 是文档生成工具。
  **不是移植缺口**，列出仅为划清边界。
- 唯一关联缺口：Rust 引擎无独立遥测面（引擎事件经 napi 回调给宿主，宿主走
  `packages/telemetry` 上报），fed19d24f0（prompt cache 探针遥测）因此只能落宿主侧。

### A8. node-sdk 内的引擎邻接双源（不是整包，是层）

- **`native/native-llm-resolver.ts`**（40KB）：TS 侧完整实现 provider/模型解析与
  `JsNativeLlmConfig` 装配，与 Rust `config/mod.rs` 的 `ResolvedNativeLlm` 双源。
  parity 只卡字段名集合（19↔17），解析语义（env 展开、别名合并、oauth 通道）两边各写一遍。
- **`config-local/`**（schema.ts 584 行 + toml.ts 32KB）：TS 侧完整 TOML 配置解析，
  与 Rust `config/mod.rs` 双源；每加一个配置面要改四层（本次 `[mcp]` 的 `JsMcpDefaults`
  透传就是活例）。
- **`mcp-oauth.ts`**（15.8KB）与 Rust `mcp/` OAuth 存储分层：§27/§34 已把授权码+PKCE
  大部分落地，但存储键等 wire 契约仍由 `packages/protocol` + TS 宿主共享。
- **判断**：中期全 Rust 目标倾向「Rust 为唯一配置解析者、TS 经 napi 反查」（方案 b），
  但影响 TUI 热加载，工作量大；DEVELOPMENT-OPTIMIZATION §4 建议先 (a) 制度化双源，
  (b) 单独立项。**本报告把它列为最大单项技术债，而非某个待移植模块。**

---

## 3. 缺口清单 B：已登记的上游行为 delta（57 条 tracked 全量）

来源：`scripts/upstream-v2-delta-allowlist.json`（verdict=tracked，本轮全量提取）+
ROADMAP §6.46 分组。**验证状态**：[已核实]=本轮回源码确认仍存在；[台账待核]=allowlist
note 含 file:line 证据但本轮未复核（验证方法随条给出）；[政策待裁]=需用户先裁决。

### B-1 核心正确性（影响会话正确性，优先）

| # | 条目（sha） | 缺什么 / 现在由谁承担 | 涉及文件 | 状态 |
|---|---|---|---|---|
| 1 | ~~压缩请求剥离动态工具上下文（`a9dc3a9af4`）~~ | **已关闭（2026-11-02 复核改判 not-applicable）**：上游的剥离点（stripDynamicToolContext）在 fork 不存在——公告消息经 delivery 进 `messages`（run_turn.rs:1989-1998）、压缩拿全量 `&messages`（:1061）、`summarization_prompt` 无过滤；fork 摘要请求是刻意的 transcript 形态（§6.9.2），与 loop 从不共享前缀，无缓存复用可丢。原 note 的「发空 tools」张冠李戴（上游 :284 的 tools 属 token 估算，#4410 未动 tools） | — | [已核实-已关闭] |
| 2 | ~~媒体降级/剥离状态不跨轮不跨压缩（`8dabf4a10e`）~~ | **已移植（2026-11-02）**：`MediaRecovery`（model + revision 键控，Stripped 带键集）在 `llm/media_budget.rs`；run_turn 回合头应用、阶梯命中时记录；EngineSession `set_history`/`clear_history` 与服务端 `record_context_truncation` 前进 revision；napi/REPL/server/子代理四路接线。压缩一侧为 transcript 形态本就无媒体 | `llm/media_budget.rs`、`turn_loop/run_turn.rs`、`session/mod.rs`、`server/engine.rs` | [已关闭] |
| 3 | ~~自动压缩保不住 prompt cache（`68a2838941`）~~ | **已移植（2026-11-02）**：预缩门禁改按真实 input limit（`compaction/mod.rs:884-921`）；压缩请求带 `used_context_tokens` + `max_context_tokens`（估算 + 2% margin，http.rs:253 封顶生效）；`Degraded(usize)` 钉住 413 时的降级计数、可见媒体 >2 才重降级（run_turn.rs:1286-1326）。未移植：cache 探针遥测（归 fed19d24f0 裁决） | `compaction/mod.rs`、`llm/media_budget.rs`、`turn_loop/run_turn.rs` | [已关闭] |
| 4 | ~~跨进程会话占用锁（`3079e9515f`）~~ | **已移植（2026-11-02）**：`session/lock.rs` 全量移植 v2 #4246——锁文件 `<home>/locks/sessions/<sid>.lock`（tmp+hardlink 原子发布）、holder JSON 与 v2 同字段名（双引擎互读）、pid/hostname/boot-time 存活判定、`.reclaim` sidecar 单赢者回收、`KIMI_CODE_SESSION_LOCK=false` 开关、IO 失败降级继续、`session.occupied` 逐字文案。持锁三缝：server `execute()`（含 ACP/REST/queue 全部 turn 入口）、undo/compact 路由（409 `SESSION_OCCUPIED=40903`）、napi/CLI pump；写守卫 `append_wire_event_json`（check-only 而非 v2 的 acquire-retain，差异已注释）；takeover 双落点（REST `:takeover` + napi `session_takeover_lock` + session-handle 包装）。刻意差异：无 `process.on('exit')` 清扫（显式释放点 + stale 回收兜底）、无 per-host display name（走 v2 通用文案臂）。判定：`a_turn_on_a_foreign_held_session_is_refused_as_occupied`、`the_wire_write_guard_refuses_a_foreign_held_session`、lock.rs 十三测 | `session/lock.rs`、`server/engine.rs`、`session/mod.rs`、`server/route_sessions.rs`、`napi_bindings/session.rs` | [已关闭] |
| 5 | ~~子代理 profile allowlist 断链（ROADMAP §6.40 扫描项）~~ | **已移植（2026-11-02）**：三跳管道贯通——`SubagentProfileWire.subagents`/`JsSubagentProfile.subAgents`/`SubagentDefinition.subagents`/`AgentProfile.subagents` 四跳字段 + build-handle.ts 显式投影（顺带修复 discovered profile 的 system_prompt 正文断流）；`subagent/allowlist.rs` 移植 v2 四件套（`subagent_allowlist_for`/`profile_can_delegate`/`without_delegating_targets`/`AGENT_TYPE_NOT_ALLOWED` 文案），内建 'agent' 默认表 `['coder','explore','plan']`；spawn 闸口在 agent/swarm 两侧**定义检查之后**（保插件 profile 宿主回退），Agent 工具描述按 `SpawnCatalog` 过滤公告。判定：`the_spawn_gate_enforces_the_callers_allowlist`（拒绝文案 + extras 放行 + 声明收窄三臂）、`agent_def_lists_discovered_types_the_gate_admits`、allowlist.rs 三测 | `subagent/allowlist.rs`、`subagent/manager.rs`、`tools/agent_tool.rs`、`napi_bindings/`、`node-sdk/build-handle.ts` | [已关闭] |
| 6 | ~~子代理继承父 profile/工具集/模型对齐请求前缀（`f0bc988bbc`）~~ | **已移植（2026-11-02）**：fork 臂 profile = `requested ?? caller_profile`（原为落 coder），model/thinking 经 `instance_llm_of(caller)` 逐字继承调用方、跳过 pool 解析以保请求前缀逐字节一致；`fork_incompatibility` 门在绑定之前。fork 架构差异：闸口置于定义检查之后（插件 profile 永远到不了快照，闸口在前会掐断宿主回退）；`primary` 在 fork 门上不再豁免，fork 显式 model 一律拒绝（对齐上游 `args.model !== own.modelAlias` 的从严方向）。判定：`fork_inherits_the_callers_profile`、`a_pool_bound_callers_fork_binds_the_same_pool_entry`、`fork_members_inherit_the_callers_model` | `tools/agent_tool.rs`、`tools/swarm_tool.rs`、`subagent/manager.rs`、`subagent/fork.rs` | [已关闭] |
| 7 | ~~子代理限定配置的 model pool（`e3efbb9d3b`）~~ | **已移植（2026-11-02）**：`secondary.rs` 删 `Forced` 臂与隐式 `primary`（'primary' 变为普通非法别名，reserved 文案用 v2 现行版）；残留 `force` 键在引擎 `config/mod.rs` 与宿主 `native-llm-resolver.ts` 双层警告忽略（不报错）；单条目池/裸 `default_model` 隐藏 model 参数并隐式绑定；无池时**任何**显式 model（含 primary）报 CONFIG_INVALID。判定：`agent_primary_is_an_invalid_alias_now`、`agent_without_a_pool_rejects_{an_explicit_model,primary_too}`、`agent_single_entry_pool_binds_the_default_when_model_is_omitted`、`test_swarm_rejects_an_explicit_model_without_a_pool` | `subagent/secondary.rs`、`config/mod.rs`、`node-sdk/native-llm-resolver.ts`、`subagent/fork.rs` | [已关闭] |
| 8 | ~~MCP 工具公告先于首次发现（`7ccac2b45b`）~~ | **已移植（2026-11-02）**：fork 的注入 provider 为同步签名，等价落点为注入 pass 前的门禁——`run_turn.rs:1156-1160` 在 `step_num == 0` 先 await `toolset.wait_for_mcp_discovery_before_disclosure()`（toolset.rs:221-236，开关+capability+manager 三条件）；`wait_for_initial_load` 随全部 connect settle 解封（各有连接超时上界） | `turn_loop/run_turn.rs`、`tools/toolset.rs` | [已关闭] |
| 9 | ~~LLM 历史思考键按当前模型探测 + Responses usage 回退（`88c3becefc`）~~ | **已移植（2026-11-02）**：(1) 思考键——`openai.rs:229` `resolve_reasoning_key` 三档 declared>detected>history（:242 history 新→旧、限可写方言）；:790-806 流内检测首个可写方言（声明键抑制、probe-only 不记录）；`http.rs:160-183` 进程级 `DETECTED_REASONING_KEYS`（对齐上游 registry 生命周期，键 `base#model` 对齐 `modelKey`，first-seen-wins）——实例级 Mutex 会随每回合 pipeline 重建丢失，进程级 store 才等价。(2) usage——`openai_responses.rs:449-463` 逐事件读取：普通事件 nested ?? 顶层，completed/incomplete 只读嵌套。e2e 判定：`http::tests::a_detected_dialect_replays_unmarked_history_on_the_next_request`（跨请求重放） | `llm/openai.rs`、`llm/openai_responses.rs`、`llm/http.rs` | [已关闭] |
| 10 | ~~中断时取消在途视频上传（`c15638b0a1`）~~ | **已移植（2026-11-02）**：fork 的 read_media 工具不上传视频（`tools/read_media.rs:387-441` 一律内联），上传集中在 `budgeted_request`（`run_turn.rs:357-407`）——`resolver.resolve` 外套 select 竞速取消令牌，取消即丢弃 future（连带在途 HTTP 上传），biased 保证已取消回合不起上传；调用点（:1241-1264）走干净 Aborted。v2 只装视频工具臂，fork 一处竞速覆盖全部媒体族 | `turn_loop/run_turn.rs` | [已关闭] |
| 11 | ~~`KIMI_CODE_LLM_HEADERS_TIMEOUT_MS`（`c151d5a3d0`）~~ | **已移植（2026-11-02）**：`http.rs:38-56` 解析（空白=不限、非法值 `config.invalid` 复用上游文案）；:869-893 非 google 协议把 reqwest send future 包 `tokio::time::timeout`（send 在响应头到达时 resolve = undici headersTimeout 等价点，同上游三臂范围、google 不装）；超时文案 `llm transport error headers_timeout` 可重试，`config.invalid` 经 :1349-1354 新守卫不可重试。undici 的代理 dispatcher 分支在 reqwest 模型下不需要 | `llm/http.rs` | [已关闭] |
| 12 | ~~wire 记录无协议版本（ROADMAP §6.41.2 / §1 板块 10）~~ | **已移植（2026-11-02；本行原描述已过期——版本列与 event_store 闸口先于本轮存在）**：本轮补齐剩余缺口——`append_wire_event_json` 写入打戳 `WIRE_PROTOCOL_VERSION="1.5"`；`refuse_newer_wire_version` 抽为共享闸口，四个 journal 切片 helper（`get_wire_events`/`interaction`/`prompt`/`task_wire_events`）与既有冷投影闸口同闸；NULL 行（列出现前）按本版本读取，现有 DB 零行为变化。五级迁移链判为不适用：fork 的表自始只有 1.5 形状，上游迁移变换的是其自身历史 JSONL 形状。判定：`append_wire_event_stamps_the_current_protocol_version`、`the_wire_readers_refuse_a_newer_protocol_version`、`null_protocol_versions_read_as_the_current_version` | `session/sqlite_store.rs`、`native/event_store/mod.rs` | [已关闭] |

### B-2 会话/服务端生命周期

| # | 条目（sha） | 缺什么 | 涉及文件 | 状态 |
|---|---|---|---|---|
| 13 | 空闲会话回收（`366fafbf92`） | kap-server 的超时回收（环境变量可配）；Rust server 无 | `server/engine.rs`、`server/mod.rs` | [台账待核] |
| 14 | close/archive 时驱逐广播事件状态（`5710c5496a`） | fork 仅空闲+容量驱逐（note 指 `hub.rs:541`） | `server/hub.rs` | [台账待核] |
| 15 | transcript 冷重建 ordinal 对齐 wire id + wedged 标记（`67325c1b25`） | 冷重建轮次序号与循环看门狗诊断 | `server/transcript/` | [台账待核] |
| 16 | 大会话恢复性能对等（`256ce61f0c`） | fork 仅压缩锚点，无基准——**先做基准再谈移植** | `session/`、`storage/` | [台账待核]（验证方法=基准先行） |
| 17 | 默认工作区（`47002256a0`+`e5e56bc5fe`） | 无项目会话的默认工作区置于 home 下 | `server/`、宿主 | [台账待核] |
| 18 | `/reload` 后通知代理外部变更（`18c6d64272`） | changeNotifier 服务与变更快照（`changeNotifier` 全仓零命中，本轮已核实）；**依赖 fs watch 地基（B-4 #27）** | `server/`、`injection/` | [已核实] |
| 19 | remote control 设备管理/删除（`c69bd6c5a0`/`b510fa9aec`） | protocol 面只有 `GET/POST /v1/remote-control`（本轮已核实 `rest/remoteControl.ts`），设备 list/delete 端点在 fork 协议面根本不存在；Rust `remote_control.rs`（51KB）有设备注册但无管理路由 | `protocol/src/rest/remoteControl.ts`、`server/remote_control.rs` | [已核实] |
| 20 | 会话评分规则共享 + 服务端路由（`ce0834a8f3`） | 新产品包 | 新增 | [台账待核] |

### B-3 工作区 / fs / git 面

| # | 条目（sha） | 缺什么 | 涉及文件 | 状态 |
|---|---|---|---|---|
| 21 | 多根工作区注册表 + source folders（`093e8f46c4` 等 5 条：`3ed0e68f3b` skill 跨目录合并、`7d72ae3965` trust 可配置、`67045e5ec5` 重复 source folder 去重、`bfdfde1e20` fs 搜索覆盖全部 source folders） | **架构级**：Rust 是否引入 source folders 概念要先裁决 | `server/`、`config/`、`skills/` | [政策待裁] |
| 22 | `fs:list` 支持 `allow_ignored_globs`（`5b93669767`） | fork 只读 path（`allow_ignored_globs` 零命中，本轮已核实） | `server/fs_routes.rs` | [已核实] |
| 23 | 目录范围 git status（`1853dae4ff`） | fork 仅 work_dir 全量 | `server/fs_routes.rs` 或 tools | [台账待核] |
| 24 | 文件树符号链接（`e2ad1dcf21`） | 缺 `allow_external_symlinks`、扩张预算与根前缀检测（`metadata()` 已跟随链接） | `server/fs_routes.rs` / fs 列表 | [台账待核] |

### B-4 交互体验 / 提示词 / 通知

| # | 条目（sha） | 缺什么 | 状态 |
|---|---|---|---|
| 25 | Plan 模式交还用户、每个 plan 都审阅（`0325b755db`） | **删 EnterPlanMode，工具/协议面改动**；`EnterPlanMode` 仍存在于 `tools/plan_mode.rs:213`（本轮已核实） | [政策待裁，已核实现状] |
| 26 | NotifyUser：中途进度文字 + 按轮次计静默（`7091138f5e`）；title + 宿主进度展示（`0a3a2fbafa`）；无更新面板的宿主停 nudge（`4fbe065442`，v2 双输入门禁 vs fork 单输入，note 指 `tools/mod.rs:1547` 一带） | 显示层 + 引擎节流 | [台账待核] |
| 27 | 文件系统 watch 默认开启（`c7dd84124a`） | v2 的 watch 地基在 fork 完全缺席（`KIMI_CODE_WATCH` 全仓零命中，本轮已核实）；ROADMAP §6.25 记 v2 `src/runtime/` capability 层在 fork 不存在 | [已核实]——**这是 B-2#18、AGENTS.md 变更通告两个缺口的共同前置** |
| 28 | 中断提醒随 undo 移除（`e3bf50c083`） | v2 五行：`ownerPromptId` 挂到 undo 锚点 | [台账待核] |
| 29 | UserPromptSubmit hook 输出折进 prompt 消息（`06ebfc821e`） | v2 不再产独立 `hook_result` 用户消息；fork 该 hook 仍只观察（§42 剩余亦记此） | [台账待核] |
| 30 | 工作区信任汇总面板（`395d537237`） | 激活摘要取代裸 MCP 命令打印（TUI 侧为主） | [台账待核] |
| 31 | ~~select_tools 区分会话失效/策略禁用/未知并解释连续失败（`e4c99a3bad`）~~ | **已移植（2026-11-02；本行原描述已过期）**：五桶分类与 v2 顺序一致（policyDenied→loadable→staticCallable→staleLoaded→unknown）在 `tools/select_tools.rs::evaluate_load_tools`；失败记账 `SelectionFailureTracker`（STOP_RETRY_THRESHOLD=2、混合批次诚实计数、环境指纹重置）挂 `tools/mod.rs` session 级 `selection_failures`；空 loadable 一次性公告在 `tools/toolset.rs::take_disclosure_announcement`；策略源只接全局 `[tools]` 层（v2 的 profile/session/workspace 三层 deny 源在 fork 无对应物，allowlist note 已登记）。判定：`test_policy_denied_buckets`、`test_stale_loaded_is_not_reported_as_available`、`test_failure_tracking_escalates_and_resets`、`select_tools_remembers_loaded_names_and_announces_them` | [已关闭] |
| 32 | 插件技能限定名 `pluginId:skill` + 放宽校验 + 插件命令优先（`3f0857eb7d`） | 技能注册面 | [台账待核] |
| 33 | WaitFor 另一半（`20a2cea72f`） | 90s 上限已移植（`tools/task_tools.rs:28`）；「新输入结束等待」一半未做 | [台账待核] |
| 34 | 待办提醒两组上游改写（`47a518ed4e`/`da5918c0c8`） | §18 已移植 todoListReminder；这两条是后续的按步数计数与文案放宽 | [台账待核]（注意与工作区未提交的 todo 提醒改动可能交叠，动手前先 diff） |
| 35 | 子代理停止后回传部分输出（`93137cad20`）、Tools 列表按可用性过滤（`85077a4190`） | 子代理收尾语义 | [台账待核] |
| 36 | 2.4.0 prompt/工具描述成批改写（`cca9bae3d7`）+ 按请求类型决定交付物（`39d149eb64`）+ 验证范围限定本次改动（`3154ec41f0`） | 触及 `prompt/system.md` 与工具描述，**需先定 prompt 政策**（工作区 `prompt/system.md` 恰有未提交改动，动手前必须先 diff） | [政策待裁] |
| 37 | tower 两簇（`01ef64ea92` 并发/锁/per-spawn 模型；`c9f01f0738` mission 确定性编排）+ 09af3b483f 六簇硬化（tower wake 中断、live-agent 保护等） | 并入既有 tower 集群 | [台账待核] |
| 38 | import-from-cc-codex 技能适配新布局（`759958afdf`） | 内置技能内容 | [台账待核] |
| 39 | 统一 /models schema（`9f0693becb`）+ 模型迁移元数据（`0327e8e9a8`）+ VS Code 登录区域推断（`273ddb077a`）+ global 遥测端点（`da4e2f57a3`） | 见 A3（宿主 oauth 面） | [已核实] |
| 40 | prompt cache 探针前缀一致性遥测（`fed19d24f0`） | 宿主遥测面；ROADMAP §19 曾建议跳过遥测，**裁决** | [政策待裁] |
| 41 | MCP 瞬时失败自动重连（`4be3f0b741`）+ #4492 重连状态去重（`0c63065fbd` 载体） | **与 AGENTS.md「刻意无重连」政策冲突** | [政策待裁] |
| 42 | SSH/Docker 远程执行环境（`5f3ab5fac8`） | 与保留的 kaos 冲突（见 A5） | [政策待裁] |
| 43 | hooks 在会话工作目录运行（`c1459a6184`） | hook 载荷 `cwd` 取进程 cwd（`external_hooks.rs:718` 本轮已核实仍是 `std::env::current_dir()`）；spawn 侧支持 per-hook `cwd`（:834-847），缺的是会话 cwd 注入 | [已核实] |

---

## 4. 不一致清单：同一能力 TS/上游 vs Rust 的行为差异

「以谁为准」的裁决原则沿用项目铁律：**上游 v2 行为定义规格**；fork 有意偏离必须在代码注释
+ 台账双登记。

| # | 差异 | 两侧位置 | 以谁为准 / 修哪边 | 状态 |
|---|---|---|---|---|
| 1 | Anthropic 缓存断点：Rust 发 4 处（system/末工具/尾块/stable-history），上游发 3 处；stable 位是 fork 自加 | `llm/anthropic.rs:164,192,239,254` vs 上游 `anthropic.ts:999,1040,1083` | 上游为准；已裁决「冗余但无害」，若收敛删 `anthropic.rs:184-192` 一段 | 已登记接受（ROADMAP §1 板块 2） |
| 2 | Plan 门禁判据基础不同：fork 看 `args.get("path")`（取不到即放行），上游看 `execution.accesses` 的每条写访问；两条上游测试用例在 fork 会失败（无 path 应否决、混合写访问应否决） | `tools/plan_mode.rs:58` vs 上游 `planService.ts:259-270` | 上游为准，修 Rust：判据换 accesses | **未修**（ROADMAP §1 上游测试对照批） |
| 3 | cron：① `minute_of_hour` 用显式 `tz_offset_minutes` vs 上游宿主本地时区；② 缺 `oneShotMaxMs>0` 守卫（常量恒 90s，不可达）；③ 停机窗口 fork 静默丢弃 vs 上游合并补发一次 | `cron/jitter.rs:16-112`、`cron/mod.rs:24-25,467-473` | ①② 登记即可；③ 留待裁决（三条修复路径各自受阻，§1 cron 行详录） | 部分待裁 |
| 4 | 重试抖动上界含端点：`fastrand::u64(0..=delay/4)` vs `Math.random()` 不含；差 1ms | `turn_loop/retry.rs:70-77` | 上游为准；无行为意义，登记 | 已登记（§1 对照批） |
| 5 | 默认免审工具名单是上游超集（多 `ListDirectory`/Lsp/`memory_*`/`Tower*`） | `permission/mod.rs:42-100` | fork 有意，代码注释已写明 | 已登记接受 |
| 6 | fs 错误码在失败点用字符串字面量而非 `error_codes` 常量（`FS_PATH_NOT_FOUND` 值 40409 只被单测引用） | `server/fs_routes.rs:920,924` vs `server/envelope.rs:29` | 修 Rust 用常量；低价值 | 未修（§6.42.5 登记） |
| 7 | `ToolExecuteResponse.note` 无 napi/ACP 消费者，已本地化文本用户不可见 | 引擎发 `tool.native` 事件；`sdk-rpc-client-native.ts` 只转 `content`/`is_error` | 需协议改动，单独排期 | 未修（AGENTS.md:108-114） |
| 8 | Informational footers 仍英文（"Total lines in file: N." 等） | `tools/mod.rs` Read/Grep 结果构造 | 在 hardcoded-rust-allowlist 记为 `deferred` 类（被计数而非描述） | 已登记待办 |
| 9 | 系统提示词：fork 18 变量 vs 上游 13（fork 多 `runtime_notes`/`memory_section`/`memory_listing`/`profile`/`preferences`），另有 fork 独有段落（`# Untrusted content`、`# Search`、安全强化句 `system.md:108`） | `prompt/builder.rs:313-335`、`prompt/system.md` vs 上游 82 行模板 | **逐条裁决未完成**（哪些保留、哪些删） | 待裁（§1 板块 6） |
| 10 | goal：`blocked_streak` 未实现（快照恒 null）；`GoalContext` 不带 `terminal_reason`；「agent 已关闭」无对应钩子；SDK 无预算设置 RPC | `goal/`、`napi_bindings/params.rs` | 上游为准，补 Rust + napi | 未修（§41 剩余） |
| 11 | hook payload 不带 agent id，多代理并发时外部脚本无法区分来源 | `tools/external_hooks.rs` 各 `notify_*` | 上游语义补齐（v2 payload 有 agent 上下文） | 未修（§42/§43 剩余） |
| 12 | native 宿主态（`meta.plan`/`meta.goal`）与引擎 workspace state 不随 undo 回滚；server checkpoint 仍是 workspace 全局栈 | `sdk-rpc-client-native.ts` undo 路径、`server/mod.rs` | 上游为准；§30 已把 state 改会话级，此项是剩余尾巴 | 未修（§22.3/§30.4 剩余） |
| 13 | AGENTS.md 磁盘变更通告（`agents_md_change` variant）缺失 + ~~Bash 操作数目录分支缺失~~（**2026-11-02 已解**：`host/parse_bash` 回调 + `node-sdk/bash-target-dirs.ts` walker；降级臂=cwd-only 与 v2 同形） | `injection/mod.rs:177` 只注册 `agents_md`（零命中已核实） | 上游为准；磁盘变更通告前置 = B-4#27（fs watch） | 操作数目录分支 [已关闭]；变更通告未修（§1 板块 5） |
| 14 | kimi anthropic trait / prompt_cache_key / 动态 completion budget | §25 已落地 | — | **已关闭**（§25） |
| 15 | usage 语义（Anthropic 重复减 cache、OpenAI total 偏小、Google 缓存段） | §24 已修 | — | **已关闭**（§24） |
| 16 | 台账矩阵称「external hooks 只覆盖 8/20 事件」 | `external_hooks.rs:11-14` 文件头列全 20 种且有 `notify_*` 触发点 | — | **台账过期**（§10.27 与 §42/§43 已补齐；矩阵行未回填，本轮已核） |

---

## 5. 补完计划（分阶段）

与「可读、可对齐、高性能」一致：先解政策阻塞，再做高影响低成本，地基类单独成批，
每批增量可编译、静态门禁全绿即停。

### 阶段 0：政策裁决（不碰代码，解锁后续批次）

| 裁决项 | 影响的缺口 | 选项 |
|---|---|---|
| MCP 自动重连 vs 既定「无重连」政策 | B-41 | 维持现状记 not-applicable / 移植上游重连（退避+状态迁移） |
| 远程执行环境 vs kaos 处置 | B-42、A5 | 退役 kaos 死代码 + Rust 实现远程环境 / 都不要 |
| Plan 模式协议（删 EnterPlanMode） | B-25 | 跟上游删 / 保留现协议 |
| prompt 成批改写（2.4.0 sync） | B-36 | 逐段裁决 fork 独有段落（#9 一并） |
| 多根工作区是否进 Rust | B-21 | 架构级，单独 RFC |
| 会话全文检索是否需要 | A4 | 决定 minidb 处置 |
| prompt cache 遥测 | B-40 | 跳过 / 落宿主遥测 |

### 阶段 1：高影响 × 低成本（每条独立成批）

1. ~~**B-3/B-31 select_tools 失败分类**~~ **已落地（2026-11-02）**：见 B-4 表 #31。
2. **B-43 hooks 会话 cwd**：`HostCallbacks`/pipeline 把会话 work_dir 传进
   `notify_*` 的载荷与 spawn 默认 cwd。改动面收敛在 `external_hooks.rs` + 调用点。
3. ~~**B-11 `KIMI_CODE_LLM_HEADERS_TIMEOUT_MS`**~~ **已落地（2026-11-02）**：见 B-1 表 #11。
4. **B-22 `allow_ignored_globs`**：fs:list 路由加参数。TS manifest + Rust 路由 +
   parity 自动卡一致性。
5. **B-39 global 遥测端点 + 统一 /models schema**：纯 `packages/oauth` 宿主侧，
   不碰 Rust。可先落地。
6. ~~**B-5 子代理 allowlist 三跳管道**~~ **已落地（2026-11-02）**：见 B-1 表 #5。
7. **#2 Plan 门禁判据换 accesses**：`plan_mode.rs:58` 判据基础换成 scheduled accesses，
   两条上游用例转绿。

### 阶段 2：核心正确性（需要设计，单独立项）

1. ~~**B-1/B-2/B-3 压缩与媒体三件套**~~ **已落地（2026-11-02）**：见 B-1 表 #1（改判
   not-applicable）/#2/#3。
2. ~~**B-4 跨进程会话锁**~~ **已落地（2026-11-02）**：见 B-1 表 #4；`0a90e2e8a2` 随之
   ported，`e7f1a99fc5` 重新适用为 tracked（剩 TUI 占用提示流）。
3. ~~**B-6/B-7 子代理前缀对齐与 model pool**~~ **已落地（2026-11-02）**：见 B-1 表 #6/#7。
4. ~~**B-8 MCP 公告等首次发现**~~ **已落地（2026-11-02）**：见 B-1 表 #8。
5. ~~**B-12 wire 协议版本列**~~ **已落地（2026-11-02）**：见 B-1 表 #12。

### 阶段 3：地基类（成本最高，收益面广）

1. **A1 bash 解析**：~~两条路择一~~ **已走 napi 宿主回调路并关闭 AGENTS.md 操作数目录
   （2026-11-02）**；dangerous_command 精确化（权限放宽）留作单独批次，路线（Rust 移植 /
   QuickJS 内嵌）届时再评。
2. **B-27 fs watch 地基**：引擎侧 watch 服务（`notify` crate）+ `KIMI_CODE_WATCH`
   配置面；解锁 B-18（/reload 变更通知）与 #13（AGENTS.md 变更通告）。
3. **A8 配置/LLM 双源收敛**：先 (a) 制度化（每个配置面登记 + parity 扩字段级 diff），
   再评估 (b) Rust 单源。
4. **B-13/B-14/B-15 服务端生命周期**：空闲回收、关闭驱逐、transcript 重建对齐。
5. **B-16 大会话恢复基准**：先建基准，不达标再移植 v2 优化。

### 阶段 4：产品面与 TS-only 包终态

1. A2 kosong 终态（vis-server 迁引擎 RPC 或登记为终态包）。
2. A4 minidb 处置（依阶段 0 裁决）。
3. B-19/B-20 remote control 设备管理与评分规则（protocol 面先声明）。
4. B-17 默认工作区、B-26 NotifyUser 族、B-28/B-29/B-30 提醒与 hook 折叠。
5. B-37/B-38 tower 集群与技能布局。
6. #7 `note` 协议透出、#8 英文 footer 本地化、#10/#11/#12 剩余尾巴。

### 不在本计划内（明确排除）

- `apps/kimi-code` TUI/CLI（67K 行）与 `pi-tui`：app 层，按架构留 TS。
- `packages/protocol`/`telemetry`/`i18n-*`/`normify`/`transcript`：契约与共享层，留 TS。
- `apps/vscode`、`apps/vis` 深入审计：本轮未覆盖，另立项。

---

## 6. 验收判据（静态检查为主，回归只在提交前）

| 关卡 | 命令 | 何时跑 |
|---|---|---|
| Rust 静态三件套 | `cd packages/kimi-agent && cargo check && cargo clippy --all-targets --features cli -- -D warnings && cargo fmt --check` | 每批改动后 |
| napi 代码生成 | `cd packages/kimi-agent && bun run build`（动 napi 面时必跑，宏展开对模块路径敏感） | 涉及 `napi_bindings/` 或 `#[napi]` 时 |
| TS 静态 | `bun run typecheck` + `bun run lint`（tsgo + oxlint --type-aware） | 动 TS 后 |
| 接口 parity | `bun run check:parity`（协议/工具/napi/配置/shell 五面） | 动协议面后 |
| 架构与模块 | `bun run check:architecture` + `bun run check:rust-module-deps`（后者是双向棘轮，新依赖对必须 `--update` 落基线） | 动模块结构后 |
| 台账同步 | 每条上游 delta 落地后把 allowlist verdict 从 tracked 改 ported（`bun scripts/check-upstream-v2-delta.mjs --update` 保留 verdict）；ROADMAP 对应章节回填 | 每条 delta 关闭时 |
| 聚合门禁 | `bun run check:all`（16 门禁） | 每阶段收尾 |
| 提交前最终验收 | `cargo test --no-default-features --features cli,workflow-js` + `bun run test` 全量；命中 `MEMORY.md` 三个已知脆弱项（native-harness EBUSY、tree-sitter 500KB 时间预算、state_store 并行争用）时单测复跑，不全套重跑 | 仅此环节跑回归 |

**新增缺口的标准动作**（防再静默堆积）：上游 delta 由 `check:upstream-v2-delta` 强制
triaged；TS/Rust 界面漂移由 `check:parity` 卡住；行为语义差异目前没有门禁——本报告
§4 的每一条在关闭时必须附「上游出处 sha + fork 落点 file:line + 判定测试名」三件套进
allowlist note，这是 §11 失真前科换来的纪律。

---

## 7. 本报告的验证方法与覆盖边界

**做过的**：
- 四个核心门禁单独复跑并解读输出语义（parity 的 `model 23/37` 实为 serde rename+alias
  双计数，归一化后无差集——避免了一次误报进报告）；`check:all` 16 门禁端到端 EXIT=0。
- 57 条 tracked delta 全量提取并按 §6.46 分组重组；其中 **12 条本轮回源码核实**
  （文中标 [已核实]），含 compaction 空 tools（`compaction/mod.rs:752`）、媒体局部变量
  （`run_turn.rs:800-801`）、hooks 进程 cwd（`external_hooks.rs:718`）、global 无
  telemetryEndpoint（`region.ts:70-76`）、managed /models 读扁平字段
  （`managed-kimi-code.ts:432`）、`AGENT_TYPE_NOT_ALLOWED`/`KIMI_CODE_WATCH`/
  `allow_ignored_globs`/`KIMI_CODE_LLM_HEADERS_TIMEOUT_MS`/`changeNotifier` 零命中、
  select_tools 单桶（`select_tools.rs:69-95`）、EnterPlanMode 仍存在（`plan_mode.rs:213`）、
  tree-sitter-bash 零生产消费者、kaos 仅 type 引用、模型字段无差集。
- 台账失真纠正 1 处：§1 矩阵「hooks 8/20」已过期（20 种事件全有触发点）。
- DEVELOPMENT-OPTIMIZATION.md 的 D4 疑问已闭环：「durable state 按 workspace 共享」由
  §30.1 会话级 state 关闭；「session 批准粒度」由 §10.42 关闭。

**没做的 / 覆盖不到的**：
- 45 条 tracked delta 标 [台账待核]：allowlist note 含 file:line 证据但本轮未逐条复核；
  每条给了验证方法（按符号 grep + 读 note 指的行号邻域，行号可能漂移）。
- 全部结论为**静态阅读 + 门禁输出**，未运行任何测试、基准或真实会话；运行时行为
  （如「首轮公告漏 MCP 工具」的实际可观测性）未验证。
- `apps/vis`、`apps/vscode`、`plugins/`、`scripts/prompt-optimizer`、`docs/` 未深入。
- `bun源码参考/` 未审计（已在 .gitignore，仅作 Bun 行为参考）。
- 上游测试对照只覆盖台账已读的 6 个模块；ROADMAP §1 末「上游测试规格映射」表里
  30 余行标「否」的逐条对照是后续独立工作（531 个上游测试文件）。
