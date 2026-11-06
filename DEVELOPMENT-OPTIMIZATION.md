# kimi-code fork 开发优化方案

> **已归档（2026-11-06）**：本文件是 2026-11-02 的只读静态分析快照，不再维护、不再作为
> 裁决依据。已完成的阶段与仍然开放的条目已并入 `packages/kimi-agent/ROADMAP.md`（新基点）。
>
> 快照时间：2026-11-02（Asia/Shanghai）。本文档为**只读静态分析**产物，分析期间未修改任何代码、未改动 Git 状态。
> 行号均以此为基准的快照行号；另一会话正在并行改动本仓库，引用行号前请先用符号搜索复核。
> 放置说明：`docs/AGENTS.md` 规定 `docs/` 根目录只保留 `index.md` 与站点设施、内部报告不放 `docs/`；`reports/` 又被 `.gitignore:31` 整体忽略（0 个已跟踪文件）。为让本文档可版本化、可长期维护，故置于仓库根目录，与 `MEMORY.md` / `AGENTS.md` 同级。

---

## 1. 现状总览（关键数字）

| 维度 | 数字 | 证据 |
|---|---|---|
| Rust 引擎规模 | 295 个 `.rs` 文件，245,503 物理行 | `packages/kimi-agent/src` |
| Rust 测试规模 | 约 2,839 个 `#[test]`/`#[tokio::test]` | 全 crate 属性计数 |
| 最大 Rust 生产文件 | `acp/mod.rs` 4,860 行；`napi_bindings.rs` 4,287 行；`tools/agent_tool.rs` 3,564 行；`tools/github.rs` 3,170 行 | 物理行统计 |
| 最大 Rust 测试文件 | `server/tests.rs` 7,746 行；`turn_loop/run_turn/tests.rs` 6,876 行 | 物理行统计 |
| 最大 TS 文件 | `sdk-rpc-client-native.ts` 4,587 行（已从 6,347 行拆出 `sdk-client/helpers.ts` 1,247 行、`build-handle.ts` 1,116 行）；`tui/kimi-tui.ts` 2,800 行；`tool-call.ts` 2,751 行 | `packages/node-sdk/src`、`apps/kimi-code/src` |
| 非测试 Rust 中 `unwrap()/expect()` | 3,447 处，最密集：`acp/mod.rs` 331 处、`storage/state_store.rs` 180 处、`tools/agent_tool.rs` 96 处 | 正则计数 |
| 门禁脚本 | 12+ 个 `check:*`/`scan:*` 脚本，全部在 CI 运行（`.github/workflows/ci.yml:223-337`） | 根 `package.json:48-66` |
| 上游 delta 台账 | 62 tracked / 24 not-applicable / 13 ported / **0 pending** | `scripts/upstream-v2-delta-allowlist.json` |
| 台账文档 | `packages/kimi-agent/ROADMAP.md` 9,523 行（43+ 个日期章节）；`AGENTS.md` 821 行 / >64KB | 物理行统计 |
| 未提交改动（快照时） | 7 文件 +99/−14：MCP `[mcp]` 全局默认值经 napi 透传引擎（`JsMcpDefaults`），`McpManager::set_defaults` 改同步锁 | `git diff --stat` |

执行链路（与 `architecture.json` 一致）：CLI → `KimiHarness`（`packages/node-sdk/src/kimi-harness.ts:83`）→ `SDKRpcClientNative`（`native/sdk-rpc-client-native.ts:219`）→ napi 进程内调用 → `EngineSession` → `turn_loop/run_turn.rs`。分层约束 `app > sdk > engine > shared`，无环，由 `check-architecture-drift.mjs` 门禁保证。

---

## 2. 现状诊断

### 2.1 结构性问题（按对「可读、可对齐、高性能」的伤害排序）

**S1. Rust 侧仍有 6 个 2,500+ 行的生产文件，拆分战役只打完上半场。**
已完成：`server/mod.rs`、`turn_loop/run_turn.rs`、`tools/mod.rs`、`server/dispatch.rs`（14 个域方法 + `route_miss` 护栏）。
未拆：`acp/mod.rs` 4,860（单文件内含 `AcpServer` 全部 impl，`acp/` 目录已有 `permission.rs`/`channel.rs`/`events_map.rs`/`types.rs`/`question.rs`/`builtin_commands.rs` 六个兄弟文件，说明拆分范式已确立，mod.rs 是最后一块）、`napi_bindings.rs` 4,287（30+ 个 `#[napi]` 导出、`JsRunTurnParams` 巨型参数对象、MCP manager 静态注册表混在一处）、`tools/agent_tool.rs` 3,564、`tools/github.rs` 3,170、`permission/mod.rs` ~2,960、`storage/state_store.rs` ~2,900。
依据：物理行统计；拆分先例见 git log 与 `ROADMAP.md` §6.45。

**S2. 测试文件比生产文件更失控。**
`server/tests.rs` 7,746 行、`turn_loop/run_turn/tests.rs` 6,876 行。测试巨文件的伤害是具体的：定位一个用例要全文搜索；两个会话并行改同一测试文件必冲突（当前正是多会话工作模式）。

**S3. `napi_bindings.rs` 是「边界层即上帝文件」。**
4,287 行里同时有：协议参数对象（`JsRunTurnParams` 等 13+ 个 `#[napi(object)]`，`napi_bindings.rs:824-1247`）、会话生命周期导出（`create_engine_session` :2698、`session_enqueue_turn` :2912 等）、MCP OAuth 导出（:3077-3145）、静态注册表 `MCP_MANAGERS`（:1551）。每次协议加字段（如本次未提交改动加 `JsMcpDefaults`，:1068-1083）都要进这个文件，是所有 TS↔Rust 改动的公共冲突点。

**S4. 配置解析双源：TS 与 Rust 各读一份 TOML，职责未收敛。**
TS 侧 `packages/node-sdk/src/config-local/schema.ts`（584 行）与 Rust 侧 `packages/kimi-agent/src/config/mod.rs`（1,800 行非空白）各自解析配置。本次未提交改动是典型症状：为让引擎看到 `[mcp]` 段，只能在 TS 侧解析后新增 `JsMcpDefaults` 逐字段透传（diff 注释自陈「Without this field the napi transport had no way to see the `[mcp]` section at all」）。每加一个配置面都要改四层：TS schema → napi 参数对象 → Rust 绑定 → Rust 消费方。
判断：这是当前边界设计下最痛的重复，但**不是**「删一边」能解决的——TUI/CLI 自身也消费 TS 侧配置。属于「需要裁决的架构岔路」，见 §4 阶段 2 与 §6 待验证 V3。

**S5. LLM 客户端双实现并存：`packages/kosong`（TS）vs `kimi-agent/src/llm`（Rust）。**
`kosong` 在 `architecture.json:83-92` 登记为 engine 层、依赖 kimi-agent，被 `vis-server`、`oauth` 使用；Rust 引擎自有完整 `llm/`（`http.rs` 2,572 行、`anthropic.rs`、`openai.rs` 等）。主链路已全 Rust，kosong 是过渡残留还是 vis/oauth 的终态依赖，台账中未见裁决。
判断：中期「全 Bun + 全 Rust」目标下这必须有一个明确结论，否则每次 LLM wire 变更（如 ROADMAP §24/§25 这类）都要问「kosong 跟不跟」。

**S6. 台账文档自身成为维护负担。**
- `ROADMAP.md` 9,523 行、43+ 个日期章节，且自身多次记录「台账失真」：§11 整章是台账核查轮（allowlist 反向指针 12 条错 9 条、死引用、幻影证据）；§6.44 抽样发现 11 处错误「含 1 处阻断了工作」。单一巨型 append-only 文件已被自己的历史证明不可靠。
- `AGENTS.md` 821 行 / >64KB，文件自己在第 18-19 行承认「tooling truncates this file when it is injected as project context」——核心规范被注入截断是制度性风险。
- i18n 叶子计数是手工快照（`AGENTS.md:68-76` 自陈「no gate compares it」），已发生过期。

### 2.2 性能隐患

**P1. `runShellCommand` 每次调用 `probeShellPath()`，绕过已有的实例缓存。**
`sdk-rpc-client-native.ts:2413` 直接 `probeShellPath()`；而同类已有 `resolvedShellPath()` 缓存（:260-261、:431-435）。Windows 上若无 pwsh，`probeShellPath` 每次 spawn 两个 `where` 进程（`native-llm-resolver.ts:958-981`，无模块级 memoize——已 grep 确认）。TUI 里每条 `!` 命令都走这条路。
依据：直接代码阅读。严重度：低-中（非会话创建热路径，但属已修问题的同款残留，参照 `MEMORY.md` 记录的 ~470ms/会话教训）。

**P2. `dist-web` 预构建产物出现同内容双 hash 文件。**
`apps/kimi-code/dist-web/assets/` 下存在成对文件，如 `abnfDiagram-VRR7QNED-BMMI9BxQ.js` 与 `abnfDiagram-VRR7QNED-CJZC9dED.js`、`chunk-2U5K7J3B` 双份等，是多次异源构建产物堆叠的痕迹。该目录不可从 kimi-web 重建（硬约束），所以堆积只会增不会减。是否全部仍被 `index.html`/manifest 引用——**待验证**（见 §6 V1）。

**P3. `bun源码参考/` 24,711 个文件位于仓库根、未跟踪且未被 `.gitignore` 覆盖。**
证据：`git status --short` 显示 `?? bun源码参考/`；`.gitignore` 无对应条目。后果：每次 `git status` 全量扫描、编辑器/ripgrep 索引膨胀、agent 检索噪声。

**P4. 测试基础设施在 Windows 上的已知脆弱点（既影响性能也影响可信度）。**
`MEMORY.md:25-45` 记录三个：node-sdk `native-harness.test.ts` 清理阶段 EBUSY（~520 文件并行跑满重试预算）、`tree-sitter-bash` 500KB heredoc 时间预算测试在负载下超时、Rust `state_store` 测试并行争用间歇失败。三者都是「全量回归必跑」模式下的固定噪声源，与「静态检查为主、回归只在提交前跑」的纪律直接冲突。

### 2.3 缺陷与遗留清单（按严重度；均为快照时点的开放状态）

| # | 严重度 | 条目 | 证据位置 |
|---|---|---|---|
| D1 | 高 | `acp/mod.rs` 331 处 `unwrap/expect`。ACP 是 stdio 协议面，unwrap 命中恶意/畸形输入即整进程 panic，且 ACP 面向外部 IDE 客户端，属攻击面。其中有多少可达外部输入——待验证（§6 V2） | 计数统计；`acp/mod.rs` 全文 |
| D2 | 高 | 三个环境脆弱测试在特定条件下必红，已知两年无根治方案，只能「单独重跑」规避 | `MEMORY.md:25-45`；ROADMAP §10.24 |
| D3 | 中 | Node→Bun 残留 5 项：vis 构建脚本、locale-keys 加载 tsx/esm、vscode-publish 用 pnpm、文档 npm install 路径、`#/*` tsconfig paths 无门禁 | `ROADMAP.md` §23.3（:8760-8767） |
| D4 | 中 | 2.1.1 审计剩余项中可能仍未消化的部分（§20.3 列出时未修；§21-§26 已修掉危险命令解析、undo、MCP 配置分层等，但 durable state 按 workspace 共享、session 批准粒度两项在后续章节未见关闭记录） | `ROADMAP.md` §20.3（:8656-8660） |
| D5 | 低 | `ToolExecuteResponse.note` 无 napi 消费者，已本地化文本用户不可见 | `AGENTS.md:108-114`（Known gaps） |
| D6 | 低 | `probeShellPath` 热路径残留（同 P1，既算缺陷也算性能） | `sdk-rpc-client-native.ts:2413` |

> D3/D4 源自台账记录，未逐条回源码复核（台账自身有 §11 记录的失真前科），执行前必须先验证，验证方法见各阶段条目。

### 2.4 工程流程观察

**F1. 门禁体系完整但缺本地统一入口。** CI 跑全部 12+ 门禁（`ci.yml:223-337`），但本地没有一个 `bun run check:all`；`publish` 脚本（`package.json:71`）只含 typecheck/lint/sherif/test/build/lint:pkg。新贡献者（或 agent）本地验证只能靠记忆逐条跑。低风险、低成本可补。

**F2. pre-commit 只跑 `check:architecture`**（`package.json:133`），`check:rust-module-deps` 与 `scan:hardcoded:rust` 不在内，Rust 侧回归只能在 CI 发现。这是「CI 红而本地绿」类问题（ROADMAP §14 已记录一次指纹行尾事故）的结构性温床。是否收紧需权衡提交延迟，列为决策点而非直接建议。

---

## 3. 优化方案（分阶段，与用户优先级一致：先 Rust 模块整理，后缺陷修复）

每阶段给出：目标 / 涉及文件 / 改动类型 / 预期收益 / 风险与回滚 / 验收标准。所有阶段的验收以**静态检查为主**（`cargo check`、`cargo clippy --all-targets --features cli -- -D warnings`、`cargo fmt --check`、`bun run typecheck`、`bun run lint`、相关门禁脚本），回归测试（`cargo test`、`vitest run`）只作为该阶段提交前的最终验收跑一遍。

### 阶段 0：仓库卫生（零风险，先行）

- **0.1** `.gitignore` 增加 `bun源码参考/`；删除根目录 `.vitest-run.log`、`.vitest-run2.log`（已被 `*.log` 规则覆盖，纯残留）。
  收益：`git status` 与全文检索即刻减负。回滚：还原一行 ignore。
- **0.2** 修 P1/D6：`sdk-rpc-client-native.ts:2413` 改用既有 `this.resolvedShellPath()`。
  收益：消除每条 `!` 命令最多 2 次进程 spawn。风险：行为差异仅在「运行中途改了 PATH」场景，可忽略；回滚为单行还原。
  验收：`bun run typecheck` + `bun run lint`；node-sdk 相关测试随提交前回归跑。

### 阶段 1：Rust 模块整理（第一优先级，对齐「可读、可对齐」）

沿用既有拆分纪律：只移动不改逻辑、保持原执行顺序、必要时加内部护栏（`route_miss` 先例）、每拆一个文件立刻跑 `bun run check:rust-module-deps`（基线双向棘轮，`scripts/rust-module-deps-baseline.json`）。

- **1.1 拆 `napi_bindings.rs`（4,287 行）→ `napi_bindings/` 目录。**
  切分方案（沿现有注释块边界）：`params.rs`（全部 `#[napi(object)]` 参数对象，:824-1247）、`session.rs`（会话生命周期导出，:2698-3200）、`mcp.rs`（MCP OAuth 与 manager 注册表，:1551、:3077-3145）、`turn.rs`（`run_turn_rust` :1300 及回调管线 :219-236）、`mod.rs` 仅保留 re-export 与初始化。
  收益：TS↔Rust 协议的每次改动（当前最高频改动面）从 4,287 行上帝文件变成按域定位；直接消除多会话并行改协议时的合并冲突。
  风险：纯移动，napi 宏展开对模块路径敏感是已知坑——验收必须含 `cd packages/kimi-agent && bun run build`（napi 代码生成）而不只是 `cargo check`。
- **1.2 拆 `acp/mod.rs`（4,860 行）。**
  沿 `acp/` 已有子文件范式继续抽：会话管理、prompt 循环、事件投影各成文件，`mod.rs` 只留 `AcpServer` 骨架与分发。D1 的 331 处 unwrap **不在本阶段处理**（拆分只移动不改编译行为），单独进阶段 3。
  验收：静态三件套 + `check:rust-module-deps` + ACP 相关测试随回归跑。
- **1.3 拆 `tools/agent_tool.rs`（3,564）与 `tools/github.rs`（3,170）。**
  agent_tool 按子命令域（spawn/resume/send/tower 桥）切；github 按 API 域（pr/issue/repo）切。
- **1.4 拆测试巨文件：`server/tests.rs`（7,746）、`turn_loop/run_turn/tests.rs`（6,876）。**
  按被测模块镜像命名（如 `server/tests/history.rs`、`server/tests/ws.rs`），与阶段 1.1-1.3 分开提交，避免同 PR 双重移动难审。
- **1.5 二线大文件排期（不阻塞 1.1-1.4 提交）：** `permission/mod.rs`、`storage/state_store.rs`、`llm/http.rs`、`session/sqlite_store.rs`、`server/engine.rs`（2,468）、`session/mod.rs`、`subagent/manager.rs`。每个独立成批，节奏按并行会话的冲突面让行。

阶段 1 总验收：`cargo check` / `clippy -D warnings` / `fmt --check` / `check:rust-module-deps` / `check:architecture` 全绿；提交前 `cargo test --no-default-features --features cli,workflow-js` 全量一遍（注意 `MEMORY.md` 记录的 state_store 间歇项，命中则单测复跑而非全套重跑）。

### 阶段 2：TS 边界与双源裁决（承接阶段 1，需用户裁两项决策）

- **2.1 `sdk-rpc-client-native.ts` 拆分收尾（4,587 行，目标 ≤2,000）。**
  已完成约 28%（拆出 `sdk-client/helpers.ts`、`build-handle.ts`）。继续按既有方向抽：会话生命周期（`createSession`/`buildHandle` :330-428 一带）、`runShellCommand` 及 shell 事件桥（:2402-2500）、配置装配。验收：`typecheck` + `lint` + node-sdk 测试随回归。
- **2.2 【决策点】S4 配置双源。** 两个选项：(a) 维持双解析，但把「TS 解析 → napi 透传」固化为制度并给每个配置面登记（本次 `JsMcpDefaults` 模式的开销就是每个面四层改动）；(b) 让 Rust 引擎成为唯一配置解析者，TS 侧经 napi 反查。中期全 Rust 目标倾向 (b)，但 (b) 影响 TUI 配置热加载路径，工作量大。**建议先 (a) 制度化，(b) 单独立项评估**。验证方法：清点 `config-local/schema.ts` 与 `config/mod.rs` 的字段交集，列出全部「TS 有、Rust 无」的面（本次 `[mcp]` 只是第一个）。
- **2.3 【决策点】S5 kosong 终态。** 选项：(a) vis-server/oauth 迁到 Rust 引擎 RPC，kosong 退役；(b) kosong 保留为独立 LLM 包并登记为终态。验证方法：grep kosong 的全部消费方（已知 `architecture.json` 登记 vis-server、oauth），评估 vis-server 改走引擎会话的改动面。

### 阶段 3：缺陷修复（第二优先级）

- **3.1 D1：ACP unwrap panic 面审查。** 先做可达性分类（§6 V2），只把「外部输入可达」的 unwrap 改为错误返回；锁中毒类沿用项目现行 `unwrap_or_else(|e| e.into_inner())` 惯例（见本次未提交 `mcp/manager.rs` diff）。不追求清零，追求「协议面不 panic」。
- **3.2 D4：逐条核销 §20.3 剩余。** 先回源码验证 durable state 共享与 session 批准粒度两项是否已被 §30/§31（会话级 durable state、goal continuation）覆盖——§30.1 标题即「会话级 durable state」，疑似已修但未回写 §20.3。验证后关账或在台账补状态。
- **3.3 D3：Node→Bun 残留 5 项。** 按 §23.3 清单逐项落地；其中「`#/*` tsconfig paths 门禁」应做成新 `check:*` 脚本而非一次性修复。
- **3.4 D2：三个脆弱测试的处置。** 原则：不改重试预算（`MEMORY.md` 明令）。可行路径：给三个测试打独立标签，在全量回归中串行化或隔离 shard；CI 矩阵已分 5 shard（`ci.yml:77`），把 EBUSY 测试固定到独立 shard 是低成本方案。**是否值得做需先量测**：统计近 20 次 CI 运行中这三项的红率，<5% 则维持现状只更新文档。

### 阶段 4：性能与卫生收尾

- 4.1 P2：`dist-web` 资产引用核查后清理孤儿文件（§6 V1）；若能确认全部仍被引用，则改为在 `build` 脚本中加「构建前清空 assets」一步防继续堆叠。
- 4.2 会话创建路径复查：`detect_js_runtimes` 的 `OnceLock` 缓存（`environment.rs`）与 shell 缓存已落地，本项仅做回归性 spot-check：构造 10 次会话创建计时，确认无新引入的 per-session spawn。验证方法：`native-llm-resolver.ts` 与 `sdk-rpc-client-native.ts` 全量 grep `spawn|exec`，逐个标注调用频率。
- 4.3 F1：根 `package.json` 增加 `check:all` 聚合脚本（顺序：架构 → parity → i18n → 硬编码 → rust-module-deps → upstream-delta → locale 系列），不改变 CI，只统一本地入口。

### 阶段 5：文档与台账治理（与阶段 1 并行可做，互不冲突）

- **5.1 ROADMAP 归档切分。** 以 2026-10-03 台账核查轮（§11）为界：§1-§11 封存为 `ROADMAP-ARCHIVE-2026Q3.md`，正文保留开放条目索引 + §12 起的新章节。依据：§11 之前的条目已全部闭环或被后续章节覆盖，封存不改变信息量，把单文件从 9,523 行压到约 2,000 行。风险：台账章节交叉引用多，切分后必须全量 grep 修正 `§x.y` 引用；回滚为文件还原。
- **5.2 AGENTS.md 瘦身。** 把「Engine-side i18n」「Upstream Merge Policy」等长章节抽到 `AGENTS/` 目录单文件，主文件留索引与硬约束，目标 <200 行（注入窗口内）。依据：文件自陈被截断（`AGENTS.md:18-19`）。
- **5.3 i18n 叶子计数入门禁。** 在 `check-engine-i18n-parity.mjs` 增加计数断言，消除手工快照（`AGENTS.md:68-76` 的已知失真面）。

---

## 4. 优先级排序原则与排期理由

1. **阶段 0 先行**：零风险、十分钟级、为后续所有阶段清障。
2. **阶段 1（Rust 模块整理）优先于阶段 3（缺陷修复）**：与用户裁定一致。附加理由：D1/D3 的修复都要落在大文件上，先拆分能显著降低缺陷修复的冲突面和审查成本——拆分是缺陷修复的前置投资。
3. **阶段 2 的两个决策点卡在阶段 1 之后**：配置与 LLM 双源的最终形态会影响 napi 参数对象的归属，先拆完 `napi_bindings.rs` 再动协议面，避免二次移动。
4. **阶段 5 与阶段 1 并行**：台账/规范文档不碰代码，与代码拆分无冲突面，且 ROADMAP 切分最好在下次大规模记录前完成。
5. 一切改动遵循既定铁律：对齐上游官方意图优先于 fork 自创；拆分时「只移动不改逻辑」；热路径进程 spawn 必须缓存。

## 5. 验收标准汇总（静态检查为主）

| 关卡 | 命令 | 适用阶段 |
|---|---|---|
| Rust 编译/静态 | `cargo check` + `cargo clippy --all-targets --features cli -- -D warnings` + `cargo fmt --check`（`packages/kimi-agent`） | 1、3 |
| napi 代码生成 | `cd packages/kimi-agent && bun run build` | 1.1、2 |
| TS 静态 | `bun run typecheck` + `bun run lint` | 0、2、3、4 |
| 架构门禁 | `bun run check:architecture` + `bun run check:rust-module-deps` | 1、2 |
| 引擎专项门禁 | `bun run check:engine-i18n`、`scan:hardcoded:rust`、`check:parity` | 1、3 |
| 提交前最终验收 | `cargo test --no-default-features --features cli,workflow-js` + `bun --bun run test`（仅此环节跑全量回归；命中 `MEMORY.md` 三个已知脆弱项时单测复跑，不全套重跑） | 所有阶段的提交点 |

## 6. 待验证清单（判断依据与验证方法）

- **V1（P2/4.1）** `dist-web/assets` 双 hash 文件是否仍被引用。方法：取 `dist-web/index.html` 与运行时 manifest 的全部引用 hash 集合，与目录文件求差集。未验证前不要删。
- **V2（D1/3.1）** `acp/mod.rs` 331 处 unwrap 的外部输入可达性。方法：以 `AcpServer` 的 stdio 读入点为源，沿调用链标注 unwrap 的数据来源；只改可达集。预估可达子集远小于 331。
- **V3（S4/2.2）** 配置双源的完整字段差集。方法：对比 `config-local/schema.ts` 的 zod schema 叶子与 `config/mod.rs` 的 serde 结构体叶子，生成三面清单（仅 TS / 仅 Rust / 双侧）。
- **V4（D3/3.3）** §23.3 五项在快照后的最新状态（台账记录日 2026-10-31，另一会话可能在动）。方法：逐项 grep 确认，如 `scripts/check-locale-keys.mjs` 是否仍 `require('tsx/esm')`。
- **V5（D2/3.4）** 三个脆弱测试的真实红率。方法：拉近 20 次 CI 运行记录统计；红率 <5% 则不动测试，只把处置写进 `MEMORY.md`。
- **V6（S5/2.3）** kosong 的全部运行时消费方。方法：`grep -r "@moonshot-ai/kosong\|from 'kosong'"` 全仓（排除构建产物），与 `architecture.json` 登记比对。

## 7. 分析范围与局限

- 全部结论基于**静态阅读**（源码、`git diff`、`ROADMAP.md`/`AGENTS.md`/`MEMORY.md` 台账、CI 配置），未运行任何测试、构建或基准；性能数字均引用台账既有测量（如会话创建 590ms→200ms），未独立复测。
- 行号为 2026-11-02 快照物理行；分析过程中已观察到并发会话在改动本仓库（如未提交的 `JsMcpDefaults` 批次），引用前请按符号复核。
- D3/D4 清单来自台账转录，未逐条回源码验证（台账有 §11 记录的失真前科），执行前必须先过 §6 对应验证项。
- 未覆盖：`apps/vis`、`apps/vscode`、`packages/normify`、`plugins/` 的深入审计；`scripts/prompt-optimizer`；`bun源码参考/` 的内容定性（只确认其规模与跟踪状态）。
- 未做：任何运行时 profiling。阶段 4.2 的计时验证设计已给出，但未执行。
