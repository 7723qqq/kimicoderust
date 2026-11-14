
### features/** 逐文件行为等价续审（2026-11-15）

续自 `tower/tools/mission/missionTool.ts`，按 v2 目录顺序核对剩余 32 个文件。只修改当前项的独立 Rust 文件；不改旧容器、不接线、不提交，不运行编译、测试、格式化或门禁。

#### 已审文件

| v2 文件 | Rust 落点 | 核对结果 |
| --- | --- | --- |
| `tower/tools/plan/plan.md` | `features/tower/tools/plan/plan.md` | 文本逐行一致，无源码修改 |
| `tower/tools/plan/plan.ts` | `features/tower/tools/plan/plan.rs` | strict 输入、必填字段、missions/scope 非空、可选字段拒 null、kind 枚举及 schema 描述均一致，无源码修改 |
| `tower/tools/rebase/rebase.md` | `features/tower/tools/rebase/rebase.md` | 文本逐行一致，无源码修改 |
| `tower/tools/rebase/rebase.ts` | `features/tower/tools/rebase/rebase.rs` | mission 必填、strict 输入与 schema 一致，无源码修改 |

| `tower/tools/review/review.md` | `features/tower/tools/review/review.md` | 文本逐行一致，无源码修改 |
| `tower/tools/review/review.ts` | `features/tower/tools/review/review.rs` | 输入校验一致；将 `acceptance_tip` 的 schema 描述改为 v2 原文，其他 schema 不变 |
| `tower/tools/send/send.md` | `features/tower/tools/send/send.md` | 文本逐行一致，无源码修改 |
| `tower/tools/send/send.ts` | `features/tower/tools/send/send.rs` | 必填字段、strict 输入、可选字段拒 null 与 schema 描述均一致，无源码修改 |
| `tower/tools/spawn/spawn.md` | `features/tower/tools/spawn/spawn.md` | 文本逐行一致，无源码修改 |
| `tower/tools/spawn/spawn.ts` | `features/tower/tools/spawn/spawn.rs` | strict 输入、kind 枚举、可选字段拒 null、worker/reviewer 条件必填与 trim 校验及 schema 描述均一致，无源码修改 |
| `tower/tools/status/status.md` | `features/tower/tools/status/status.md` | 文本逐行一致，无源码修改 |
| `tower/tools/status/status.ts` | `features/tower/tools/status/status.rs` | strict 输入、limit 可省略但拒 null、正整数校验及 schema 描述均一致，无源码修改 |
| `tower/tools/teardown/teardown.md` | `features/tower/tools/teardown/teardown.md` | 文本逐行一致，无源码修改 |
| `tower/tools/teardown/teardown.ts` | `features/tower/tools/teardown/teardown.rs` | strict 输入、三个可选字段拒 null 及 schema 描述均一致，无源码修改 |
| `tower/tower-worker-overlay.md` | `features/tower/tower-worker-overlay.md` | 文本逐行一致，无源码修改 |
| `tower/tower.ts` | `features/tower/tower.rs` | 工具名及顺序、worker profile、flag/service id、permission pin、enter failure/result、exit reason 与失败文案均一致，无源码修改 |
| `tower/towerRateLimit.ts` | `features/tower/towerRateLimit.rs` | snapshot 的 budget/inflight/blockedUntil 映射与 service id 一致，无源码修改；接口实现行为在下一文件核对 |
| `tower/towerRateLimitService.ts` | `features/tower/towerRateLimitService.rs` | 四个常量、governor 的缩容/恢复/重置、pause/success、budget/acquire/release/snapshot/reset 语义及错误文案均一致，无源码修改 |
| `tower/workerProfile.ts` | `features/tower/worker_profile.rs` | 工具名及顺序、profile 描述、whenToUse、CODER_ROLE 与 overlay 拼接均一致，无源码修改 |
| `usage/usageFeature.ts` | `features/usage/usageFeature.rs` | 将 agent model 标识改为 v2 定义的 `usage`，新增 `sessionUsageService` id，并在本文件组装 agent-model 与 session service 两项 Feature 贡献；未注册或接线 |

#### 边界未合（待搬运）

| v2 出处 | 独立 Rust 文件与旧容器 | 旧实现偏差 |
| --- | --- | --- |
| `tower/tools/plan/planTool.ts:17-67` | `features/tower/tools/plan/planTool.rs`；`tools/tower/mod.rs:338-390` | 独立文件只有定义和旧入口委托。旧入口没有 v2 的 `Planning N tower mission(s)` 执行描述、`approvalRule`、`sideEffect=read` 和 tower active 守卫；直接解析旧 `PlanArgs`，未复用独立文件的 optional-null 与数组非空校验；成功尾文案仍为 `plus reviewers for the branches`，缺少 build/artifact 分流及 artifact 人类验收说明 |
| `tower/tools/rebase/rebaseTool.ts:14-91` | `features/tower/tools/rebase/rebaseTool.rs`；`tools/tower/mod.rs:1040-1112` | 独立文件只有定义和旧入口委托。旧入口没有执行描述、`approvalRule`、`sideEffect=write`；查找 mission 未 trim，unknown-mission、up-to-date、conflict、rebased 文案均不同，conflict 额外返回 isError，成功文案仍称记录 clean-rebase review waiver，且缺少 artifact 重新 TowerComplete/人类验收与 build 新评审指引 |

| `tower/tools/review/reviewTool.ts:17-143` | `features/tower/tools/review/reviewTool.rs`；`tools/tower/mod.rs:1623-1773` | 独立文件只有定义和旧入口委托；本次已让定义使用对齐后的独立 schema。旧入口没有 `Submitting tower review for …` 执行描述、`approvalRule`、`sideEffect=read`；直接反序列化旧 `TowerReviewInput`，未执行 status 正则、merge 枚举和 optional-null 校验；输出未追加 v2 `deliverTowerMessage` 的 delivery 文本 |
| `tower/tools/send/sendTool.ts:15-76` | `features/tower/tools/send/sendTool.rs`；`tools/tower/mod.rs:1352-1424` | 独立文件只有定义和旧入口委托。旧入口没有发送执行描述、`approvalRule`、`sideEffect=read`，直接解析旧 `TowerSendInput` 而接受 optional null；未走 v2 `deliverTowerMessage`，改为在部分 tower 发送场景追加 `Agent(resume=…)` 提示，投递结果文案与覆盖分支不同 |
| `tower/tools/spawn/spawnTool.ts:83-636` | `features/tower/tools/spawn/spawnTool.rs`；`tools/tower/mod.rs:451-1018` | 独立文件只有定义和旧入口委托。旧入口没有 `Spawning tower …` 执行描述、`approvalRule`、`sideEffect=read` 和 tower active 守卫；旧 `SpawnArgs` 接受 optional null 且未做条件字段 trim 非空校验，并多出 user-stop、reviewer 分支存在/终态及 paused-mission 守卫；worktree 普通错误直接失败而非告警继续；提示词未读完整 mission 文件，缺少 worker/reviewer 历史、完整 checklist 与 diffBase，仍要求手动发送 review-result；成功结果的 task_id 取 agent_id，尾部进度与复活指引也不同 |
| `tower/tools/status/statusTool.ts:30-308` | `features/tower/tools/status/statusTool.rs`；`tools/tower/mod.rs:1892-1933`、`tools/tower/status_reader.rs:141-615` | 独立文件只有定义和旧入口委托。旧入口没有 `Reading tower status` 执行描述、`approvalRule`、`sideEffect=read`、`repoMutation=none`；旧 `StatusArgs` 接受 limit null 和 0，并把 0 截为 1，而 v2 拒绝非正数；状态读取与渲染沿用旧模块，inbox 没有 v2 的 1000 条读取上限，空 block details 时输出标点不同，user-stop 还额外接受多组取消原因 |
| `tower/tools/teardown/teardownTool.ts:18-83` | `features/tower/tools/teardown/teardownTool.rs`；`tools/tower/mod.rs:1213-1333` | 独立文件只有定义和旧入口委托。旧入口没有 dry-run/force 执行描述、`approvalRule`、`sideEffect=write`；旧 `TeardownArgs` 接受 optional null；state load 失败会直接报错，而 v2 只把 priorOwner 视为 undefined 并继续；非 dry-run 成功后额外清 wake、release session、发送 `tower_mode.exit` 与 telemetry，和 v2 保持 tower mode active 的行为冲突 |

#### 登记不改

| v2 出处 | Rust 现状 | 阻塞接口与偏差 |
| --- | --- | --- |
| `tower/tools/status/statusReader.ts:73-221` | `features/tower/tools/status/statusReader.rs` 仅再导出 `tools/tower/status_reader.rs:127-227,540-615` | v2 `readTowerStatus` 接收 active/cwd/agentId/tasks/concurrency 回调，保留传入 active，inbox 最多读 1000 条并返回裁剪后的 state；Rust 公开函数接收 store/caller/active_agent_ids/max_running_tasks，自行决定 active、返回完整 TowerState 且 unread 计数无该上限。v2 `readTowerStatusSummary` 是简短文本摘要，Rust 再导出的 `render_tower_status_summary` 却是完整工具 dashboard；对齐需改公开函数/结构签名及旧模块其他调用方，故本文件不单独改 |
| `tower/tools/support.ts:11-36` | `features/tower/tools/support.rs` | store、caller 与两条常量一致；但 v2 `runTowerTool` 只把 TowerProtocolError/GitError 转为 isError，其他异常继续抛出，Rust 的公开签名只返回 `Result<_, String>` 并把任意 Err 都转为 isError。表达该区分需引入跨模块错误类型并改公开函数签名，故本文件不单独改 |
| `tower/towerFeature.ts:62-96` | `features/tower/towerFeature.rs` | 13 项工具贡献顺序、domain、app/onDemand 限流服务和 enabled 时贡献 profile 一致；但 v2 assembled 判定以 flag service 为键，在构造成功后加入 WeakSet、Dispose 时删除，并有独立测试覆盖值。Rust `TowerFeature::new(enabled)` 后 `is_tower_feature_assembled(&self)` 永远只返回 enabled，未表达构造/Dispose 生命周期和测试覆盖。对齐需接入 flag service 身份与 Dispose 生命周期并改公开接口，故本文件不单独改 |
| `tower/towerOps.ts:8-68` | `features/tower/towerOps.rs` | enter/exit/inbox 类型与字段、tower/owner fold 一致；但 v2 enter/exit 均 durable 且 fold 时另发 `AgentStatusUpdated(towerMode)`，inbox-sent 为 observable，两个 state 还有 replayable schema。Rust 只有类型常量和本地 bool/owner 状态修改，没有这些事件元数据与状态更新事件。补齐需接入事件上下文、AgentStatusUpdated 和 state 定义接口并改 fold 公开签名，故本文件不单独改 |
| `tower/towerService.ts:77-950` | `features/tower/towerService.rs:1-32` | `TOWER_MODE_TOOLS`、flag veto 清单及 wake variant 一致；独立文件其余仅再导出旧 init/teardown/resume/wake 入口，没有 v2 AgentTowerService 的 enter/exit/isActive、branch lease、restore/reconcile、roster death、工具 veto、worker write/bash guard 和 inbox wake 的 batch/cancel/requeue/digest 编排。补齐需接入 DI 生命周期、事件/任务/profile/config 等多个服务并改公开服务接口和调用方，故本文件不单独改 |

### 移植进度：goal 缺失资产与 features 文件复核（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。本轮逐目录对照 v2 `features/**` 与 Rust 现有落点，包含位于 `agent/`、`tools/`、`injection/`、`skills/` 的既有实现；未改变任何注册或运行路径。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/goal/injection/goal-budget-limited-reminder.md` | `features/goal/injection/goal-budget-limited-reminder.md` | 按 v2 原文补上缺失资产；未把 `GoalStatus::BudgetLimited` 或 reminder 选择逻辑改接到该文件 |
| `features/gitMemory/tools/repo-map.md` | `tools/git_memory.rs::REPO_MAP_DESCRIPTION` | 原文已内联，保持既有实现，不重复复制 Markdown |
| `features/associativeMemory/tools/memoryGraphTool.ts` | `tools/memory_graph_tool.rs` | 已有完整输入、schema、action 分发与执行实现；feature 目录其余文件均已存在，不新增重复委托 |
| `features/helloWorld/agent/injection/helloWorldInjection.ts` | `injection/hello_world_mode.rs` | 既有落点，保持不动 |
| `features/codeRuntime/tools/run_code/run_code.md` | `tools/run_code_tool.md` | 既有资产，保持不动 |
| `features/swarm/{agent,tools}/**/*.md` | `injection/swarm_mode.rs`、`swarm/*.md` 与本轮前已有 `features/swarm/tools/agentSwarm/agent-swarm-fork.md` | 既有文本或落点，保持不动 |
| `features/skill/catalog/builtin/**/*.md` | `skills/builtin/**` | 既有内建资产，保持不动 |

其余已逐文件核对组为 `attachment`、`astron`、`btw`、`codeRuntime`、`cron`、`dateChange`、`debugEvents`、`externalHooks`、`feature*.ts`、`fileHistory`、`goal`、`helloWorld`、`lsp`、`notify`、`operationLog`、`plan`、`promptOptimizer`、`reminder`、`sessionInit`、`sessionQuery`、`skill`、`snapshot`、`spec`、`swarm`、`todo`、`tokenCounting`、`usage`、`gitMemory` 与 `associativeMemory`；本轮未见其他未映射文件。该结论只覆盖文件落点，不覆盖行为、接线或测试等价。

本组新增资产后 `cargo check --lib` 出口码 0（既有 unused-import 警告仍在）。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：tower 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `tools/tower/**`、mode-mutex 与工具注册保持不动；本组新增 v2 同名目录、模型和委托，不接 feature、flag、profile、reminder 或 DI 服务注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/tower/tower.ts` | `features/tower/tower.rs` | 新增工具名、worker profile、flag/service id、进入失败/结果、退出原因与失败文案 |
| `features/tower/towerFeature.ts` | `features/tower/towerFeature.rs` | 新增 13 个工具贡献、app/onDemand 限流服务、worker profile 与 Feature 元数据组装，不注册 recipe |
| `features/tower/flag.ts` | `features/tower/flag.rs` | 新增 `KIMI_CODE_EXPERIMENTAL_TOWER` flag 元数据、assembled 暴露判定与既有 enabled 委托 |
| `features/tower/towerOps.ts` | `features/tower/towerOps.rs` | 新增 enter/exit/inbox-sent 事件模型及 tower/owner 状态 fold |
| `features/tower/towerService.ts` | `features/tower/towerService.rs` | 再导出既有 init/teardown/resume/wake 入口，新增 mode/veto 工具清单；未新建或注册 Agent DI service |
| `features/tower/towerRateLimit.ts` | `features/tower/towerRateLimit.rs` | 再导出既有 snapshot，新增 service id |
| `features/tower/towerRateLimitService.ts` | `features/tower/towerRateLimitService.rs` | 再导出既有 `TowerRateLimit` 及 shrink/recovery/pause/default 常量，governor 逻辑保持在原实现 |
| `features/tower/bashGuard.ts` | `features/tower/bashGuard.rs` | 再导出既有 guard/parser，新增 parse 限制与 store ownership 查询 |
| `features/tower/workerProfile.ts` | `features/tower/workerProfile.rs` | 新增 worker 工具清单、role 组合与 AgentProfile 构造 |
| `features/tower/tower-worker-overlay.md` | `features/tower/tower-worker-overlay.md` | 按 v2 原文复制 |
| `features/tower/injection/towerModeInjection.ts` | `features/tower/injection/towerModeInjection.rs` | 新增 full/sparse/exit reminder 选择、cadence 与披露状态模型，不接 injector |
| `features/tower/injection/*.md` | `features/tower/injection/` 同名 `.md` | 按 v2 原文复制 3 份 reminder |
| `features/tower/protocol/index.ts` | `features/tower/protocol/mod.rs` | 声明并再导出协议映射模块 |
| `features/tower/protocol/{baseWip,concurrency,frontmatter,git,missionGate,paths,repoRoot,store,types}.ts` | `features/tower/protocol/` 同名 `.rs` | 再导出 `tools/tower/{git,concurrency,frontmatter,mission_gate,paths,store,types}.rs` 既有实现 |
| `features/tower/tools/{support,delivery}.ts` | `features/tower/tools/{support,delivery}.rs` | 新增 store/caller/error-result 辅助，再导出既有 send/wake 交付路径 |
| `features/tower/tools/*/*.ts` | `features/tower/tools/` 各同名目录的 `.rs` | 补 13 个工具的输入模型、service id、既有 schema、ToolInfo 与执行器委托；`statusReader.ts` 再导出既有状态读取 |
| `features/tower/tools/*/*.md` | `tools/tower/descriptions/*.md` | 已有 13 份工具描述，新增 Tool 文件直接 `include_str!` 引用，不复制资产 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明 `tower`。新文件无源码注释；`cargo check --lib` 出口码 0（既有 unused-import 警告仍在）。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：skill 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `skills/mod.rs`、`tools/skill.rs`、`prompt/skills_renderer.rs` 与 `skills/builtin/**` 保持不动；本组新增 v2 同名目录和映射，不重复扫描、合并、加载或注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/skill/skill.ts` | `features/skill/skill.rs` | 新增 activation/prompt 输入输出模型与服务 id |
| `features/skill/skillService.ts` | `features/skill/skillService.rs` | 再导出既有目录实现并映射 AgentSkillService |
| `features/skill/skillFeature.ts` | `features/skill/skillFeature.rs` | 新增 feature 名称、agent/onDemand、工具元数据 |
| `features/skill/skillOps.ts` | `features/skill/skillOps.rs` | 新增 skill.activated payload/event 与 trigger 模型 |
| `features/skill/prompt.ts` | `features/skill/prompt.rs` | 再导出既有 listing renderer，新增 user/model loaded-block 渲染 |
| `features/skill/tools/skill.ts` | `features/skill/tools/skill.rs` | 新增工具输入模型、服务 id、深度错误与既有 schema |
| `features/skill/tools/skillTool.ts` | `features/skill/tools/skillTool.rs` | 新增到既有 execute_skill/execute_skill_with_depth 和定义的委托 |
| `features/skill/tools/skill.md` | `features/skill/tools/skill.md` | 按 v2 原文复制 |
| `features/skill/catalog/{registry,skillRoots,fileSkillDiscovery,inMemorySkillDiscovery,skillDiscovery,skillSource,types}.ts` | `features/skill/catalog/` 同名 `.rs` | 再导出既有 scan roots、descriptor、discovery 与类型 |
| `features/skill/catalog/parser.ts` | `features/skill/catalog/parser.rs` | 再导出既有 frontmatter parser |
| `features/skill/catalog/{builtinSkillSource,userFileSkillSource}.ts` | `features/skill/catalog/` 同名 `.rs` | 再导出既有内建/文件发现 |
| `features/skill/catalog/configSection.ts` | `features/skill/catalog/configSection.rs` | 新增 3 个 section、env 与 builtin enabled 委托 |
| `features/skill/catalog/errors.ts` | `features/skill/catalog/errors.rs` | 再导出 kimi-core 既有 skill error domain 与 5 个错误码 |
| `features/skill/catalog/builtin/{builtin,registry,sub-skill}.ts` | `features/skill/catalog/builtin/{builtin,registry,subSkill}.rs` | 再导出既有内建定义与 sub-skill 根名 |
| `features/skill/catalog/builtin/{check-kimi-code-docs,custom-theme,import-from-cc-codex,mcp-config,update-config,write-goal}.ts` | `features/skill/catalog/builtin/` 对应 camelCase `.rs` | 内嵌既有同名 Markdown，不复制资产 |
| `features/skill/session/{skillCatalog,skillCatalogData,skillCatalogService}.ts` | `features/skill/session/` 同名 `.rs` | 再导出既有目录、descriptor、scope 与 service 映射 |
| `features/skill/workspace/{explicitFileSkillSource,extraFileSkillSource,configuredSkillRootWatcher,environmentSkillDiscovery,workspaceSkillCatalogService,rootFileSkillSource,pluginSkillSource,workspaceSkillCatalog}.ts` | `features/skill/workspace/` 同名 `.rs` | 再导出既有 workspace/plugin 目录实现与 service 映射 |
| `features/skill/catalog/builtin/**/*.md` | `skills/builtin/**/*.md` | 已有内建及 sub-skill 资产，保持不动 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：todo 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 todo item/service/ops/reminder 与执行器保持不动；本组只补 feature 和工具目录，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/todo/todoFeature.ts` | `features/todo/todoFeature.rs` | 新增 feature 名称、agent scope、工具名/domain 元数据 |
| `features/todo/tools/todo-list/todo-list.ts` | `features/todo/tools/todoList/todoList.rs` | 新增可选字段输入模型、到 TodoItem 的规范化、服务 id 与既有 schema |
| `features/todo/tools/todo-list/todoListTool.ts` | `features/todo/tools/todoList/todoListTool.rs` | 新增到既有定义和执行的委托及 next-step hint |
| `features/todo/tools/todo-list/todo-list.md` | `features/todo/tools/todoList/todo-list.md` | 按 v2 原文复制工具描述 |

`features/todo/mod.rs` 增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：reminder 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 reminder service/types 保持不动，本组只补 feature 和 system-reminder 映射，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/reminder/reminderFeature.ts` | `features/reminder/reminderFeature.rs` | 新增 feature 名称与 agent scope 元数据 |
| `features/reminder/systemReminder.ts` | `features/reminder/systemReminder.rs` | 再导出 prefix/suffix，新增 wrap 委托与 content 解包 |

`features/reminder/mod.rs` 增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：goal 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 goal 状态、scheduler、injection 与四个执行入口保持不动；本组补 goal、errors、feature、deadline service 映射、共享序列化/prompt 和四个工具目录，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/goal/goal.ts` | `features/goal/goal.rs` | 再导出 GoalReasonInput/ResumeGoalInput |
| `features/goal/errors.ts` | `features/goal/errors.rs` | 再导出 kimi-core 既有 goal error domain 与 8 个错误码 |
| `features/goal/goalFeature.ts` | `features/goal/goalFeature.rs` | 新增 feature 名称、app/onDemand deadline 与四个工具元数据 |
| `features/goal/goalDeadlineSchedulerService.ts` | `features/goal/goalDeadlineSchedulerService.rs` | 再导出既有 monotonic scheduler、handle 与 trait |
| `features/goal/tools/serialize.ts` | `features/goal/tools/serialize.rs` | 新增去 goalId 的 model 序列化 |
| `features/goal/tools/outcome-prompts.ts` | `features/goal/tools/outcomePrompts.rs` | 新增完成/受阻 prompt 及 turns/elapsed/tokens 格式化 |
| `features/goal/tools/create-goal/{create-goal.ts,createGoalTool.ts}` | `features/goal/tools/createGoal/{createGoal.rs,createGoalTool.rs}` | 新增输入/schema 与既有 create 定义执行委托 |
| `features/goal/tools/get-goal/{get-goal.ts,getGoalTool.ts}` | `features/goal/tools/getGoal/{getGoal.rs,getGoalTool.rs}` | 新增服务 id/schema 与既有 get 定义执行委托 |
| `features/goal/tools/set-goal-budget/{set-goal-budget.ts,setGoalBudgetTool.ts}` | `features/goal/tools/setGoalBudget/{setGoalBudget.rs,setGoalBudgetTool.rs}` | 新增预算输入/schema 与既有 set 定义执行委托 |
| `features/goal/tools/update-goal/{update-goal.ts,updateGoalTool.ts}` | `features/goal/tools/updateGoal/{updateGoal.rs,updateGoalTool.rs}` | 新增状态输入/schema 与既有 update 定义执行委托 |
| `features/goal/tools/*/*.md` | 上述 4 个工具目录的对应 `.md` | 按 v2 原文复制 4 份工具描述 |

`features/goal/mod.rs` 增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：notify 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 availability、nudge、flag 与工具定义保持不动；本组补 session notify、feature 和工具文件，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/notify/sessionNotify.ts` | `features/notify/sessionNotify.rs` | 新增持久化 enabled、initial 开启和最近 profile prompt 回退判定 |
| `features/notify/notifyFeature.ts` | `features/notify/notifyFeature.rs` | 新增 feature 名称、agent scope、工具 domain 与 enabled 条件 |
| `features/notify/tools/notify-user/notify-user.ts` | `features/notify/tools/notifyUser/notifyUser.rs` | 再导出既有工具名和输入模型，新增服务 id |
| `features/notify/tools/notify-user/notifyUserTool.ts` | `features/notify/tools/notifyUser/notifyUserTool.rs` | 新增输入校验、displayed/suppressed 输出与既有工具定义委托 |
| `features/notify/tools/notify-user/notifyOutput.ts` | `features/notify/tools/notifyUser/notifyOutput.rs` | 新增 delivered/suppressed 文案 |
| `features/notify/tools/notify-user/notify-user.md` | `features/notify/tools/notifyUser/notify-user.md` | 按 v2 原文复制 |

`features/notify/mod.rs` 增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：snapshot 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 snapshot store/service/list 保持不动，本组只补 feature 与描述资产，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/snapshot/snapshotFeature.ts` | `features/snapshot/snapshotFeature.rs` | 新增 feature 名称、agent scope、工具名/domain 元数据 |
| `features/snapshot/snapshot-list.md` | `features/snapshot/snapshot-list.md` | 按 v2 原文复制工具描述 |

`features/snapshot/mod.rs` 增加 feature 模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：dateChange 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `dateChangeService.rs` 保持不动，本组只补 3 个映射文件，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/dateChange/dateChange.ts` | `features/dateChange/dateChange.rs` | 再导出既有 DateDisclosure/DateInjectionDisclosure |
| `features/dateChange/disclosureBaseline.ts` | `features/dateChange/disclosureBaseline.rs` | 再导出既有 renderGeneration baseline 选择 |
| `features/dateChange/dateChangeFeature.ts` | `features/dateChange/dateChangeFeature.rs` | 新增 feature 名称与 agent scope 元数据 |

`features/dateChange/mod.rs` 增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：cron 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 cron 调度、表达式、state、config 与 `tools/cron_tools.rs` 保持不动；本组只补缺失的 feature/errors 和三个工具的同名文件，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/cron/errors.ts` | `features/cron/errors.rs` | 新增 cron.expression_invalid 常量 |
| `features/cron/cronFeature.ts` | `features/cron/cronFeature.rs` | 新增 feature 名称、agent scope、工具 domain/name 元数据 |
| `features/cron/tools/cron-create/cron-create.ts` | `features/cron/tools/cronCreate/cronCreate.rs` | 新增输入/输出模型、服务 id、50/8KiB 上限与既有 schema |
| `features/cron/tools/cron-create/cronCreateTool.ts` | `features/cron/tools/cronCreate/cronCreateTool.rs` | 新增到既有 create 定义和执行的委托及 one-shot 上限 |
| `features/cron/tools/cron-delete/cron-delete.ts` | `features/cron/tools/cronDelete/cronDelete.rs` | 新增输入模型、服务 id 与既有 schema |
| `features/cron/tools/cron-delete/cronDeleteTool.ts` | `features/cron/tools/cronDelete/cronDeleteTool.rs` | 新增到既有 delete 定义和执行的委托 |
| `features/cron/tools/cron-list/cron-list.ts` | `features/cron/tools/cronList/cronList.rs` | 新增服务 id 与既有空输入 schema |
| `features/cron/tools/cron-list/cronListTool.ts` | `features/cron/tools/cronList/cronListTool.rs` | 新增到既有 list 定义和执行的委托及 200-byte preview |
| `features/cron/tools/cron-{create,delete,list}/cron-{create,delete,list}.md` | `features/cron/tools/cron{Create,Delete,List}/` 对应 `.md` | 按 v2 原文复制 3 份短描述 |

`features/cron/mod.rs` 增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：helloWorld 缺失文件补齐（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定，也不改此前已完成的 helloWorld 真实接线。本组只补缺失的 feature 文件。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/helloWorld/helloWorldFeature.ts` | `features/helloWorld/helloWorldFeature.rs` | 新增 feature 名称、agent scope 与 onScopeCreated 元数据，不重复注册 |

`agent/helloWorld.ts`、`agent/helloWorldService.ts`、`helloWorldOps.ts`、enter/exit 与 7 个 stage 资产均已有落点；`agent/injection/helloWorldInjection.ts` 对应既有 `injection/hello_world_mode.rs`，均保持不动。`features/helloWorld/mod.rs` 只增加上述模块声明。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未写「终态」。

### 移植进度：feature 框架文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。本组补 feature 根目录的 4 个框架文件，不把现有特性自动注册进去。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/feature.ts` | `features/feature.rs` | 新增 contribution 模型、model/config/service/tool/command/profile 贡献入口、handle 与 dispose |
| `features/featureRegistry.ts` | `features/featureRegistry.rs` | 新增全局 recipe 注册、读取与测试清理 |
| `features/featureAssembly.ts` | `features/featureAssembly.rs` | 新增 assembly 服务 id 与接口 |
| `features/featureAssemblyService.ts` | `features/featureAssemblyService.rs` | 新增 app/onScopeCreated 元数据和从 recipes 构造 feature 的 assembly，不接 featureManager |

在 `features/mod.rs` 声明上述模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：swarm 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `swarm/**`、`injection/swarm_mode.rs`、`tools/swarm_tool.rs` 与 `config/mod.rs` 保持不动；本组新增同名特性目录并复用实现，不改调度、模式 veto、工具或配置注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/swarm/configSection.ts` | `features/swarm/configSection.rs` | 再导出 SwarmConfig，新增 section、2h 默认值、env 与 timeout 解析入口 |
| `features/swarm/swarmFeature.ts` | `features/swarm/swarmFeature.rs` | 新增 feature 名称、agent/session scope、onScopeCreated 与工具元数据，不注册 |
| `features/swarm/swarmOps.ts` | `features/swarm/swarmOps.rs` | 再导出既有 mode 事件，新增 state/event 类型常量 |
| `features/swarm/agent/swarm.ts` | `features/swarm/agent/swarm.rs` | 再导出 SwarmModeTrigger，新增 agent 服务 id |
| `features/swarm/agent/swarmService.ts` | `features/swarm/agent/swarmService.rs` | 再导出既有 mode registry、turn-end、veto 与事件写入 |
| `features/swarm/agent/injection/swarmInjection.ts` | `features/swarm/agent/injection/swarmInjection.rs` | 再导出既有 enter/exit reminder、注册与 baseline 扫描 |
| `features/swarm/agent/enter-reminder.md` | `injection/swarm_mode.rs::SWARM_MODE_ENTER_REMINDER` | 已有内嵌文本，保持不动 |
| `features/swarm/agent/exit-reminder.md` | `swarm/exit-reminder.md` | 已有，保持不动 |
| `features/swarm/session/agentRunBatch.ts` | `features/swarm/session/agentRunBatch.rs` | 再导出既有完整 batch scheduler |
| `features/swarm/session/sessionSwarm.ts` | `features/swarm/session/sessionSwarm.rs` | 新增 task/result 别名、run args 与服务 id |
| `features/swarm/session/sessionSwarmService.ts` | `features/swarm/session/sessionSwarmService.rs` | 再导出既有 registry/run/launcher/lifecycle |
| `features/swarm/tools/agent-swarm/agent-swarm.ts` | `features/swarm/tools/agentSwarm/agentSwarm.rs` | 新增公开输入模型、服务 id 与既有 schema 入口 |
| `features/swarm/tools/agent-swarm/agentSwarmTool.ts` | `features/swarm/tools/agentSwarm/agentSwarmTool.rs` | 新增工具元数据、pool-aware 定义与既有执行委托 |
| `features/swarm/tools/agent-swarm/agent-swarm.md` | `swarm/agent-swarm.md` | 已有，保持不动 |
| `features/swarm/tools/agent-swarm/agent-swarm-fork.md` | `features/swarm/tools/agentSwarm/agent-swarm-fork.md` | 按 v2 原文复制 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；首次 `cargo check --lib` 发现 `agent_swarm_tool_def` 需要 pool 参数，修正定义委托后复跑出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：plan 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `tools/plan_mode.rs`、`tools/exit_plan_mode.rs`、`injection/goal_plan.rs` 与 `prompt/profiles.rs` 保持不动；本组新增独立特性目录、plan 文件服务、Todo 转换、审批纯函数与同名资产，不改现有模式切换或注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/plan/plan.ts` | `features/plan/plan.rs` | 新增 PlanData、PlanFileTarget、文件操作与服务 id |
| `features/plan/planOps.ts` | `features/plan/planOps.rs` | 新增 state、enter/cancel/exit/revision 事件模型与 main-only fold |
| `features/plan/planService.ts` | `features/plan/planService.rs` | 新增 plan path、status/clear、revision 构造、resolveFile 与写拒绝文案 |
| `features/plan/parsePlanToTodos.ts` | `features/plan/parsePlanToTodos.rs` | 新增二/三级 heading milestone 与列表 leaf/status 解析 |
| `features/plan/planToTodoConverter.ts` | `features/plan/planToTodoConverter.rs` | 新增 empty/no-structure/existing/no-agent 跳过及 converted items 结果 |
| `features/plan/exitPlanModeReview.ts` | `features/plan/exitPlanModeReview.rs` | 新增选项选择、approved 输出、non-interactive 文案及拒绝结果 |
| `features/plan/configSection.ts` | `features/plan/configSection.rs` | 新增 defaultPlanMode section 与 false 默认值 |
| `features/plan/profile/plan.ts` | `features/plan/profile/plan.rs` | 新增 plan profile、工具列表、只读元数据与角色文本，复用既有 role 常量 |
| `features/plan/injection/planModeInjection.ts` | `features/plan/injection/planModeInjection.rs` | 新增 7 个 reminder 内嵌、cadence、wasActive 与 full/sparse/reentry 选择 |
| `features/plan/planFeature.ts` | `features/plan/planFeature.rs` | 新增 feature 名称、agent scope、工具 domain/name 元数据，不注册 |
| `features/plan/tools/exit-plan-mode/exit-plan-mode.ts` | `features/plan/tools/exitPlanMode/exitPlanMode.rs` | 新增输入模型、plan source、label 校验与服务 id |
| `features/plan/tools/exit-plan-mode/exitPlanModeTool.ts` | `features/plan/tools/exitPlanMode/exitPlanModeTool.rs` | 新增到既有 exit 执行的委托、review display 与 empty-plan 文案 |
| `features/plan/tools/exit-plan-mode/exit-plan-mode.md` | `features/plan/tools/exitPlanMode/exit-plan-mode.md` | 按 v2 原文复制 |
| `features/plan/injection/plan-mode-{full,sparse,reentry,exit}-reminder.md`、`plan-mode-inline-{full,sparse,reentry}-reminder.md` | `features/plan/injection/` 同名 7 个 `.md` | 按 v2 原文复制 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：spec 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `tools/spec_mode.rs`、`injection/spec_mode.rs` 与 4 个 reminder 资产保持不动；本组新增独立特性目录、状态/文档/审批纯函数与同名工具委托，不改模式互斥、写守卫或注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/spec/spec.ts` | `features/spec/spec.rs` | 新增 spec 文件/stage/data 模型与服务 id，再导出既有目录与 required files |
| `features/spec/specOps.ts` | `features/spec/specOps.rs` | 新增 state、enter/cancel/exit/revision 事件模型与 fold |
| `features/spec/specService.ts` | `features/spec/specService.rs` | 新增 active dir、status/documentData、clear、revision 构造与 SHA-256 |
| `features/spec/exitSpecModeReview.ts` | `features/spec/exitSpecModeReview.rs` | 新增选项选择、approved 输出及 dismissed/rejected/revise 结果和遥测 outcome |
| `features/spec/errors.ts` | `features/spec/errors.rs` | 再导出既有 mode_invalid/incomplete，新增 write_denied 常量 |
| `features/spec/injection/specModeInjection.ts` | `features/spec/injection/specModeInjection.rs` | 再导出既有 injection 与 reminder，新增 wasActive key、cadence 常量和 footer |
| `features/spec/specFeature.ts` | `features/spec/specFeature.rs` | 新增 feature 名称、agent scope、工具 domain/name 元数据，不注册 |
| `features/spec/tools/enter-spec-mode/enter-spec-mode.ts` | `features/spec/tools/enterSpecMode/enterSpecMode.rs` | 新增空对象严格 schema 与服务 id |
| `features/spec/tools/enter-spec-mode/enterSpecModeTool.ts` | `features/spec/tools/enterSpecMode/enterSpecModeTool.rs` | 新增到既有 enter 执行的委托与 entered 文案 |
| `features/spec/tools/exit-spec-mode/exit-spec-mode.ts` | `features/spec/tools/exitSpecMode/exitSpecMode.rs` | 新增输入模型、label 长度/唯一/保留名校验与服务 id |
| `features/spec/tools/exit-spec-mode/exitSpecModeTool.ts` | `features/spec/tools/exitSpecMode/exitSpecModeTool.rs` | 新增到既有 exit 执行的委托、review display、incomplete/auto-approved 文案 |
| `features/spec/tools/enter-spec-mode/enter-spec-mode.md` | `features/spec/tools/enterSpecMode/enter-spec-mode.md` | 按 v2 原文复制 |
| `features/spec/tools/exit-spec-mode/exit-spec-mode.md` | `features/spec/tools/exitSpecMode/exit-spec-mode.md` | 按 v2 原文复制 |
| `features/spec/injection/spec-mode-{full,reentry,sparse,exit}-reminder.md` | `injection/spec-mode-*.md` | 已有，保持不动 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：sessionQuery 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `tools/session_query.rs` 保持不动；本组新增 v2 同名特性目录、纯查询模型/过滤/搜索/展示与既有入口委托，不重复注册 app service 或 main-only 工具。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/sessionQuery/types.ts` | `features/sessionQuery/types.rs` | 新增 session/filter/range/lineage 模型 |
| `features/sessionQuery/events.ts` | `features/sessionQuery/events.rs` | 新增 event/filter/document/hit/page/request 模型 |
| `features/sessionQuery/cursor.ts` | `features/sessionQuery/cursor.rs` | 新增 offset cursor |
| `features/sessionQuery/errors.ts` | `features/sessionQuery/errors.rs` | 再导出 kimi-core 既有 session query error domain 与 3 个错误码 |
| `features/sessionQuery/filters.ts` | `features/sessionQuery/filters.rs` | 新增 session 结果过滤、materialize 与 range 校验 |
| `features/sessionQuery/lineage.ts` | `features/sessionQuery/lineage.rs` | 新增 ancestor、root/unresolved parent 与递归 descendants |
| `features/sessionQuery/eventText.ts` | `features/sessionQuery/eventText.rs` | 新增 wire 文本字段和 content part 提取 |
| `features/sessionQuery/eventIndex.ts` | `features/sessionQuery/eventIndex.rs` | 新增 per-session revision 缓存、wire scope 与失效入口 |
| `features/sessionQuery/search.ts` | `features/sessionQuery/search.rs` | 新增 event filters、文本 regex、排序分页、query 校验与 snippet |
| `features/sessionQuery/toolContract.ts` | `features/sessionQuery/toolContract.rs` | 新增 operation/input/schema/service id 与校验文案入口 |
| `features/sessionQuery/toolInput.ts` | `features/sessionQuery/toolInput.rs` | 新增严格输入模型、session/event filter 构造、ISO 时间与 query 校验 |
| `features/sessionQuery/toolPresentation.ts` | `features/sessionQuery/toolPresentation.rs` | 新增 session/event/trace 文本展示与 cap 提示 |
| `features/sessionQuery/sessionQueryService.ts` | `features/sessionQuery/sessionQueryService.rs` | 新增服务 id 与到既有 execute_session_query/trace_lineage 的委托 |
| `features/sessionQuery/sessionQueryTool.ts` | `features/sessionQuery/sessionQueryTool.rs` | 新增工具元数据与到 service 的执行委托，不重复 main guard |
| `features/sessionQuery/sessionQueryFeature.ts` | `features/sessionQuery/sessionQueryFeature.rs` | 新增 feature 名称、app scope、工具 domain 与 main-only 元数据 |
| `features/sessionQuery/session-query.md` | `features/sessionQuery/session-query.md` | 按 v2 原文复制工具描述 |

新增 `features/sessionQuery/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：sessionInit 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `prompt/init.rs` 与 `/init` HTTP 路径保持不动；本组新增独立特性目录、取消令牌与 runner 委托，不改主 agent/subagent 装配。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/sessionInit/sessionInit.ts` | `features/sessionInit/sessionInit.rs` | 新增 session 服务 id 与 generate/cancel 接口 |
| `features/sessionInit/sessionInitService.ts` | `features/sessionInit/sessionInitService.rs` | 新增 init 常量、runner 委托、CancellationToken 取消与 run-id 清理 |
| `features/sessionInit/sessionInitFeature.ts` | `features/sessionInit/sessionInitFeature.rs` | 新增 feature 名称与 session scope 元数据，不注册 |
| `features/sessionInit/profile/init.ts` | `features/sessionInit/profile/init.rs` | 新增 init.md 内嵌并再导出既有 completion reminder |
| `features/sessionInit/profile/init.md` | `features/sessionInit/profile/init.md` | 按 v2 原文复制 init prompt |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：usage 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `session/usage.rs` 与 `agent/usage/**` 保持不动，不重复注册 agent model 或 session service。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/usage/usageFeature.ts` | `features/usage/usageFeature.rs` | 新增 feature 名称、session scope 与 UsageAgentModelDefinition 元数据 |

新增 `features/usage/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：tokenCounting 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `session/token_counting.rs` 与 `agent/token_counting/**` 保持不动，不重复注册 agent model 或 session service。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/tokenCounting/tokenCountingFeature.ts` | `features/tokenCounting/tokenCountingFeature.rs` | 新增 feature 名称、session scope 与 TokenCountingAgentModelDefinition 元数据 |

新增 `features/tokenCounting/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：lsp 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `native/lsp.rs`、`tools/lsp_tool.rs` 与其实例注册表保持不动；本组新增 v2 同名特性目录，补协议/配置/provider registry 和纯函数，执行入口委托既有工具，不重复注册 provider、feature、flag 或 config。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/lsp/lsp.ts` | `features/lsp/lsp.rs` | 新增 operation/query/result 模型与 provider/service 接口 |
| `features/lsp/lspService.ts` | `features/lsp/lspService.rs` | 新增 provider/extension 冲突检查、disposable 注销、extension 路由与 finalExtension |
| `features/lsp/lspInstance.ts` | `features/lsp/lspInstance.rs` | 再导出既有 LspClientSession，新增 4 个生命周期默认超时；实例行为仍走既有执行路径 |
| `features/lsp/lspConnection.ts` | `features/lsp/lspConnection.rs` | 再导出既有连接会话，新增 transport/abort/server error 类型 |
| `features/lsp/lspStdioProvider.ts` | `features/lsp/lspStdioProvider.rs` | 再导出既有 stdio manager/session，新增服务 id、workspace key 与 recoverable 判定 |
| `features/lsp/framing.ts` | `features/lsp/framing.rs` | 新增 raw frame 编码、增量 MessageDecoder、16 MiB 上限与 framing error |
| `features/lsp/protocol.ts` | `features/lsp/protocol.rs` | 新增 position/range/location/hover/document/initialize/JSON-RPC 模型 |
| `features/lsp/translate.ts` | `features/lsp/translate.rs` | 新增 location/locationLink/hover normalize、URI/path 转换 |
| `features/lsp/configSection.ts` | `features/lsp/configSection.rs` | 新增 LspConfig/LspServerConfig 完整字段与 section 名 |
| `features/lsp/errors.ts` | `features/lsp/errors.rs` | 再导出 kimi-core 既有 LSP error domain 与 6 个错误码 |
| `features/lsp/flag.ts` | `features/lsp/flag.rs` | 新增完整 flag 定义并再导出既有 enabled 判定，不注册 |
| `features/lsp/lspFeature.ts` | `features/lsp/lspFeature.rs` | 新增 flag 条件、session scope、工具名/domain 元数据，不注册 |
| `features/lsp/tools/lsp/lsp.ts` | `features/lsp/tools/lsp/lsp.rs` | 新增工具输入模型、服务 id 与严格 schema |
| `features/lsp/tools/lsp/lspTool.ts` | `features/lsp/tools/lsp/lspTool.rs` | 新增到既有 execute_lsp_tool 的委托及 location/hover 渲染与 100/16000 上限 |
| `features/lsp/tools/lsp/lsp.md` | `features/lsp/tools/lsp/lsp.md` | 按 v2 原文复制工具描述 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：gitMemory 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。`agent/features/git_memory` 已有 v2 逐文件落点，本组不新建重复的 `features/gitMemory`，仅补 flag 定义；既有 `tools/git_memory.rs`、`tools/memory_repo.rs` 与工具注册保持不动。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/gitMemory/cache.ts` | `agent/features/git_memory/cache.rs` → `tools/git_memory.rs` | 已有 cache path、读写、legacy 清理与索引构造，保持不动 |
| `features/gitMemory/gitMemoryEntries.ts` | `agent/features/git_memory/git_memory_entries.rs` → `tools/git_memory.rs` | 已有 memory entries 收集与渲染去重，保持不动 |
| `features/gitMemory/gitMemoryIndexLoader.ts` | `agent/features/git_memory/git_memory_index_loader.rs` → `tools/git_memory.rs` | 已有 HEAD/index 加载与刷新，保持不动 |
| `features/gitMemory/gitMemoryRenderState.ts` | `agent/features/git_memory/git_memory_render_state.rs` → `tools/git_memory.rs` | 已有 512 上限 render state，保持不动 |
| `features/gitMemory/gitMemoryService.ts` | `agent/features/git_memory/git_memory_service.rs` → `tools/git_memory.rs` | 已有 repo/signal/file/repoMap 服务，保持不动 |
| `features/gitMemory/gitReader.ts` | `agent/features/git_memory/git_reader.rs` → `tools/git_memory.rs` | 已有 git history/head/tree 读取，保持不动 |
| `features/gitMemory/projection.ts` | `agent/features/git_memory/projection.rs` → `tools/git_memory.rs` | 已有 commit/repo/tree/file/dir/signal 投影，保持不动 |
| `features/gitMemory/types.ts` | `agent/features/git_memory/types.rs` → `tools/memory_repo.rs`、`tools/git_memory.rs` | 已有 commit 与 index 模型，保持不动 |
| `features/gitMemory/flag.ts` | `agent/features/git_memory/flag.rs` | 在既有 enabled 再导出旁补 id/env/title/description/default=false/surface=both，不注册 |
| `features/gitMemory/tools/repo-map.ts` | `agent/features/git_memory/tools/repo_map.rs` → `tools/git_memory.rs` | 已有工具名、schema 与 service id 对应实现，保持不动 |
| `features/gitMemory/tools/repoMapTool.ts` | `agent/features/git_memory/tools/repo_map_tool.rs` → `tools/git_memory.rs` | 已有 RepoMap 工具定义、空输出与执行入口，保持不动 |
| `features/gitMemory/tools/repo-map.md` | `tools/git_memory.rs::REPO_MAP_DESCRIPTION` | 已有内嵌描述，保持不动，不另建重复资产 |

本组仅编辑既有 `agent/features/git_memory/flag.rs`，无新增源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：operationLog 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `tools/operation_log*.rs` 与 `config/mod.rs` 保持不动；本组新增 v2 同名特性目录，通过再导出复用实现，不重复注册 feature、flag、config 或工具捕获。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/operationLog/operationLog.ts` | `features/operationLog/operationLog.rs` | 再导出既有 view/head/record/error/retention 模型与比较逻辑 |
| `features/operationLog/operationLogStore.ts` | `features/operationLog/operationLogStore.rs` | 再导出既有内容寻址 store、锁、retention 与恢复实现 |
| `features/operationLog/repositoryViewService.ts` | `features/operationLog/repositoryViewService.rs` | 再导出既有仓库视图、capture/restore、refs/blob 与缓存实现 |
| `features/operationLog/operationService.ts` | `features/operationLog/operationService.rs` | 再导出既有 OperationService 为 AgentOperationService，并新增服务 id |
| `features/operationLog/operationSnapshotStore.ts` | `features/operationLog/operationSnapshotStore.rs` | 再导出既有只读 snapshot/file-history 投影 |
| `features/operationLog/configSection.ts` | `features/operationLog/configSection.rs` | 再导出既有配置结构、3 个 env 常量与 retention 解析，新增 section/binding 元数据 |
| `features/operationLog/flag.ts` | `features/operationLog/flag.rs` | 再导出 flag id/env，新增完整 flag 定义，不注册 |
| `features/operationLog/operationFeature.ts` | `features/operationLog/operationFeature.rs` | 新增 feature 名称、agent scope 与 onScopeCreated 元数据，不注册 |

新增 `features/operationLog/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

**2026-11-15 收口更新**：本节“再导出复用、旧文件保持不动”的记录已过期；`tools/operation_log*.rs` 五个旧文件已删除，核心类型搬入 `features/operationLog/operationLog.rs`，不再有 re-export 层。落点与迁移见下文「收口：operationLog 旧容器删除与逻辑归属」。

### 移植进度：btw 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `subagent/btw.rs` 保持不动；本组只补 v2 同名特性文件并委托既有实现，不重复注册 scope 监听或工具 veto。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/btw/btw.ts` | `features/btw/btw.rs` | 再导出既有只读工具、拒绝文案与 reminder，新增 agent label、服务 id 和接口 |
| `features/btw/btwService.ts` | `features/btw/btwService.rs` | 新增 `SessionBtwService` 包装，启动与工具拒绝均委托 `subagent/btw.rs` |
| `features/btw/btwFeature.ts` | `features/btw/btwFeature.rs` | 新增 feature 名称与 session scope 元数据，不注册 |

新增 `features/btw/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：astron 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。本组只补 flag 定义，不注册、不改 provider 设置或登录流程。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/astron/flag.ts` | `features/astron/flag.rs` | 新增 flag id/env/title/description/default=true/surface=both 完整定义 |

新增 `features/astron/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：attachment 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `kimi-core/src/primitives/errors/domains.rs` 的 attachment error domain 保持不动，不重复注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/attachment/errors.ts` | `features/attachment/errors.rs` | 再导出既有 domain 与 5 个错误码，新增保留 `AttachmentError` 名称的 Error2 构造 |

新增 `features/attachment/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：fileHistory 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `session/sqlite_store.rs`、`features/snapshot/**`、`tools/operation_log_snapshot.rs` 及 file-history HTTP 路由保持不动；本组新增独立特性目录，服务通过后端接口和可选 operation store 工作，不接 turn、tool executor、checkpoint、undo 或 HTTP 注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/fileHistory/fileHistory.ts` | `features/fileHistory/fileHistory.rs` | 新增 backup/checkpoint/state/change/content 模型、服务 id、blob prefix 与既有 SQLite 结果转换 |
| `features/fileHistory/fileHistoryOps.ts` | `features/fileHistory/fileHistoryOps.rs` | 新增 tracked/checkpoint 事件模型、phase fallback、5-turn displacement 与 state fold |
| `features/fileHistory/fileHistoryRetention.ts` | `features/fileHistory/fileHistoryRetention.rs` | 新增 workspace 串行队列、30-session retention、失败保留、drop 与 fork 后非空 blob 检测 |
| `features/fileHistory/fileHistoryService.ts` | `features/fileHistory/fileHistoryService.rs` | 新增本地文件/blob 后端、串行 capture/end、3 次读取、4 MiB 上限、path key、content/changes/turnRecorded、orphan/evict 清理、LCS diff 与 operation store 分支 |
| `features/fileHistory/fileHistoryFeature.ts` | `features/fileHistory/fileHistoryFeature.rs` | 新增 feature 名称与 agent scope 元数据，不注册 |

新增 `features/fileHistory/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0。未运行测试，未进行端到端验证，未写「终态」。

**2026-11-15 收口更新**：原 `tools/operation_log_snapshot.rs` 的 `file_history_*` 逻辑已迁入 `features/fileHistory/file_history_service.rs`，旧 snapshot 文件已删除；HTTP 路由与 `AgentFileHistoryService` 已改接。

### 移植进度：externalHooks 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `tools/external_hooks.rs` 与 `permission::HookDef` 保持不动；本组新增 v2 同名特性目录、独立数据模型和纯函数，agent/session service 再导出既有 `HookGuard`，不重复接事件监听、配置或 feature 注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/externalHooks/configSection.ts` | `features/externalHooks/configSection.rs` | 新增 hooks section、既有 HookDef 再导出与 TOML 转换入口 |
| `features/externalHooks/externalHooksFeature.ts` | `features/externalHooks/externalHooksFeature.rs` | 新增 feature 名称和 app/session/agent scope 元数据，不注册 |
| `features/externalHooks/internal/types.ts` | `features/externalHooks/internal/types.rs` | 新增事件类型再导出、HookResult/action/block/matcher 模型 |
| `features/externalHooks/internal/matchHooks.ts` | `features/externalHooks/internal/matchHooks.rs` | 新增事件索引、regex 匹配、cwd+command 去重、并发执行、camelCase→snake_case、回调与 block 判定 |
| `features/externalHooks/internal/runHook.ts` | `features/externalHooks/internal/runHook.rs` | 新增 shell spawn 选项、stdin JSON、超时/取消、stdout/stderr 上限、exit 2 与结构化 deny 解析 |
| `features/externalHooks/internal/userPrompt.ts` | `features/externalHooks/internal/userPrompt.rs` | 新增 prompt hook 文本/part 渲染、block fallback 与失败过滤 |
| `features/externalHooks/app/externalHooksRunner.ts` | `features/externalHooks/app/externalHooksRunner.rs` | 新增 runner 参数、错误事件和服务接口 |
| `features/externalHooks/app/externalHooksRunnerService.ts` | `features/externalHooks/app/externalHooksRunnerService.rs` | 新增索引加载/reload、summary、trigger/triggerBlock/fireAndForget、reload/error 订阅 |
| `features/externalHooks/agent/agentExternalHooks.ts` | `features/externalHooks/agent/agentExternalHooks.rs` | 新增 agent 服务 id 与接口 |
| `features/externalHooks/agent/agentExternalHooksService.ts` | `features/externalHooks/agent/agentExternalHooksService.rs` | 再导出既有 `HookGuard`，新增 hook.result payload/event 与 continuation state key；事件监听仍由既有路径承担 |
| `features/externalHooks/session/sessionExternalHooks.ts` | `features/externalHooks/session/sessionExternalHooks.rs` | 新增 session 服务 id 与接口 |
| `features/externalHooks/session/sessionExternalHooksService.ts` | `features/externalHooks/session/sessionExternalHooksService.rs` | 再导出既有 `HookGuard`，新增 heartbeat 间隔和 uptime 模型；生命周期监听不在本组重复接线 |

新增各级 `mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；`cargo check --lib` 出口码 0，随后移除本组一个未使用导入。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：codeRuntime 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `workflow/code_runtime.rs`、`workflow/run_code_prelude.js`、`tools/run_code.rs` 与 `tools/run_code_tool.md` 保持不动；本组新增 v2 同名特性目录及对应文件，通过再导出和委托复用既有执行器与工具，不改工具注册或 sandbox 接线。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/codeRuntime/codeRuntime.ts` | `features/codeRuntime/codeRuntime.rs` | 新增输入模型、服务 id 与既有执行结果/限制类型再导出 |
| `features/codeRuntime/codeExecutor.ts` | `features/codeRuntime/codeExecutor.rs` | 新增运行选项、退出分类和到既有 `execute_code` 的异步委托 |
| `features/codeRuntime/codeShimSource.ts` | `features/codeRuntime/codeShimSource.rs` | 新增 subcommand 常量并内嵌既有 `workflow/run_code_prelude.js`，不复制 shim |
| `features/codeRuntime/memoryGuard.ts` | `features/codeRuntime/memoryGuard.rs` | 新增既有内存上限再导出、错误类型、guard 接口与 Linux RSS 解析；实际限制仍由既有 QuickJS runtime 执行 |
| `features/codeRuntime/runCodeTool.ts` | `features/codeRuntime/runCodeTool.rs` | 新增工具元数据与到既有 `tools/run_code.rs` 的定义/执行委托，不重复执行实现 |
| `features/codeRuntime/codeRuntimeFeature.ts` | `features/codeRuntime/codeRuntimeFeature.rs` | 新增 feature 名称、工具名和 domain 元数据，不注册 |
| `features/codeRuntime/tools/run_code/run_code.md` | `tools/run_code_tool.md` | 已有，保持不动 |

新增 `features/codeRuntime/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；首次 `cargo check --lib` 发现 shim 相对路径多一级，修正后复跑出口码 0。未运行测试，未进行端到端验证，未写「终态」。

### 移植进度：session/subagent/backend 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。已有 backend、process 与父目录 `backend_compat.rs` 保持不动；本组只补 4 个缺失落点及模块声明，不接配置、flag 或 DI 注册。

| v2 文件 | Rust 落点 | 状态与动作 |
| --- | --- | --- |
| `session/subagent/backend/acpBackend.ts` | `subagent/backend/acp_backend.rs` | 已有，保持不动 |
| `session/subagent/backend/claudeCodeBackend.ts` | `subagent/backend/claude_code_backend.rs` | 已有，保持不动 |
| `session/subagent/backend/codexBackend.ts` | `subagent/backend/codex_backend.rs` | 已有，保持不动 |
| `session/subagent/backend/codexWire.ts` | `subagent/backend/codex_wire.rs` | 已有，保持不动 |
| `session/subagent/backend/subagentBackend.ts` | `subagent/backend/subagent_backend.rs` | 已有，保持不动 |
| `session/subagent/backend/subagentBackendService.ts` | `subagent/backend/subagent_backend_service.rs` | 新增既有 service 再导出与 session/onScopeCreated 元数据；实现仍在 `subagent_backend.rs` |
| `session/subagent/backend/configSection.ts` | `subagent/backend/config_section.rs` | 新增 4 个配置结构、schema 描述与 flag 布尔映射；既有 description/strip 帮助函数再导出 |
| `session/subagent/backend/backendCompat.ts` | `subagent/backend/backend_compat.rs` | 新增父目录既有 `backend_compat.rs` 再导出，不复制实现 |
| `session/subagent/backend/flag.ts` | `subagent/backend/flag.rs` | 新增 flag 字段定义，不注册 |

`subagent/backend/mod.rs` 仅增加上述模块声明。新文件无源码注释；具名 `rustfmt --check` 0，`cargo check --lib` 0。按行新增 9 条硬编码对应记录，`scan:hardcoded:rust` 为 recorded/found 4025/4025、new/stale 0，未运行 `--update`。未进行端到端验证，未写「终态」。

### 移植进度：debugEvents 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `server/debug.rs` 保持不动；本组新增独立特性目录和模块声明，不接 DI、HTTP 或 feature 注册。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/debugEvents/debugEvents.ts` | `features/debugEvents/debugEvents.rs` | 新增 subscription、bus snapshot、聚合结果、entry kind 与服务接口 |
| `features/debugEvents/debugEventsService.ts` | `features/debugEvents/debugEventsService.rs` | 新增 scope container/unit/bus 快照模型，以及去重、递归收集和 `on:` / `disposable:EventSubscription` 标签筛选 |
| `features/debugEvents/debugEventsFeature.ts` | `features/debugEvents/debugEventsFeature.rs` | 新增 feature 名称、app scope 与 on-demand 元数据，不注册服务 |

新增 `features/debugEvents/mod.rs` 并在 `features/mod.rs` 声明模块。新文件无源码注释；具名 `rustfmt --check` 0，`cargo check --lib` 0，`scan:hardcoded:rust` 仍为 recorded/found 4016/4016、new/stale 0。未进行端到端验证，未写「终态」。

### 移植进度：promptOptimizer 文件落点（2026-11-13）

按逐文件移植范围记录，不作批次闭合或「等价」判定。既有 `promptOptimizer.rs`、`promptOptimizerService.rs` 与 `prompt/profiles.rs` 保持不动；本组只补缺失文件和模块声明，不接配置、flag、session action 或 feature 注册，也不修既有实现的缺陷与注释。

| v2 文件 | Rust 落点 | 本组动作 |
| --- | --- | --- |
| `features/promptOptimizer/promptOptimizer.ts` | `features/promptOptimizer/promptOptimizer.rs` | 已有，保持不动 |
| `features/promptOptimizer/promptOptimizerService.ts` | `features/promptOptimizer/promptOptimizerService.rs` | 已有，保持不动 |
| `features/promptOptimizer/configSection.ts` | `features/promptOptimizer/configSection.rs` | 新增配置结构、正整数 timeout 与既有诊断函数再导出 |
| `features/promptOptimizer/errors.ts` | `features/promptOptimizer/errors.rs` | 新增 kimi-core 既有 error domain 再导出 |
| `features/promptOptimizer/flag.ts` | `features/promptOptimizer/flag.rs` | 新增 flag 字段定义，不注册 |
| `features/promptOptimizer/optimizerProfile.ts` | `features/promptOptimizer/optimizerProfile.rs` | 新增 `AgentProfile` 构造函数，不改既有 catalog |
| `features/promptOptimizer/promptOptimizerFeature.ts` | `features/promptOptimizer/promptOptimizerFeature.rs` | 新增 feature 名称、session scope 与条件贡献状态，不做服务装配 |

`features/promptOptimizer/mod.rs` 仅增加上述 5 个模块声明。新文件无源码注释；具名 `rustfmt --check` 0，`cargo check --lib` 0。按行新增 4 条硬编码对应记录，`scan:hardcoded:rust` 为 recorded/found 4016/4016、new/stale 0，未运行 `--update`。未进行端到端验证，未写「终态」。

### 批次：helloWorld 全量复核与真实接线（2026-11-13，终态）

本节取代此前「零痕迹」「需要宿主入口、暂未动手」的中间判定。工作区已有纯模型与 registry 不等于特性完成；本批对照 v2 特性目录及直接消费方补齐 turn injection、工具证据、HTTP 入口、状态载荷、双向 mode-mutex 和退出清理，并修正 durable event 与 telemetry 的字段混用。

| v2 依据 | 引擎落点与终态 | 判定 |
| --- | --- | --- |
| `features/helloWorld/agent/helloWorld.ts` | `features/helloWorld/agent/helloWorld.rs`：七阶段、run items、advance/waive、路径与命令 evidence 规则；只接受成功执行的 Write/Edit/Bash 信号 | 等价 |
| `agent/helloWorldService.ts` | `agent/helloWorldService.rs` + `callbacks.rs`：native/host 两个真实工具结果出口均观察证据；run updated、status、stage telemetry 与 v2 一致，telemetry 使用 `event` 而非 durable event 的 `type` | 等价 |
| `helloWorldOps.ts` | `helloWorldOps.rs`：per-agent 当前状态 registry，`hello_world_mode.enter` / `.exit`、`hello_world_run.updated` 三类 durable event 及 `agent.status.updated` 形状一致 | 等价（Rust 状态通道） |
| `agent/injection/helloWorldInjection.ts` | `injection/hello_world_mode.rs` + `turn_loop/run_turn.rs`：按 `origin.kind === "injection"`、`origin.variant === "hello_world_mode"` 扫描 disclosure；stage 变化重注、tool trigger 静默、退出一次性提醒和 trailing removal 均已接线 | 等价 |
| `enter-reminder.md`、`exit-reminder.md`、`agent/stages/*.md` | 同名资产由 `include_str!` 加载；7 个 stage 与 enter/exit 共 9 个文件均与 v2 逐字节相同 | 等价 |
| `helloWorldFeature.ts` | feature 模块注册、turn 生产注册、工具执行 hook 和 HTTP 模式入口共同承担 v2 的 scope 创建装配，不再是孤立纯函数 | 等价（装配方式映射） |
| `agent/modeMutex/modeMutexService.ts` | helloWorld 进入退出 spec/swarm/tower、保留 plan；spec/swarm/tower 进入反向退出 helloWorld；tower 对应路径释放 session ownership，不自造 teardown | 等价 |
| `agent/usage/usageEvents.ts`、`app/sessionLegacy/sessionProtocol.ts` | `server/engine.rs` 状态载荷加入 `helloWorldMode` / `helloWorldStage`；prompt 的 `hello_world_mode` 与 profile 的 `agent_config.hello_world_mode` 均接通 enter/exit、事件和配置持久化 | 等价 |

**真实接线与边界**：

1. 生产回合在 swarm injection 旁注册 helloWorld provider；恢复历史只依据结构化 origin，不用 Markdown 内容猜测模式状态。退出时仅移除末尾对应该 variant 的 reminder。
2. `NativeToolCallbacks` 的真实 Write 测试写入 `specs/demo/requirements.md` 后，run 从 requirements 推进到 design；错误工具结果不产生证据。host 工具结果走同一服务入口。
3. HTTP enter/exit 复用同一事件构造，避免无引擎 fallback 重复 telemetry；实际 `POST /api/v1/sessions/{id}:profile` 两轮切换后，enter/exit durable event 各 2 条，配置和 registry 同步变化。
4. 未修改 `packages/protocol`、`packages/node-sdk`、`apps/**`，未扩 napi 参数；仅使用现有 HTTP 布尔字段，trigger 为 v2 mode-mutex 入口对应的 `manual`。helloWorld 新增/修改 Rust 模块无源码注释，依据记录在本节。

**验证（真实出口码）**：

- `cargo test --lib -- helloWorld` → **21 passed / 0 failed**；`cargo test --lib -- hello_world` → **8 passed / 0 failed**；完整 injection 过滤复跑 **7/7**，扩展后的真实 HTTP/profile 用例单跑 **1/1**。
- 关联回归：turn-loop **123/123**、tower **136/136**（不是先前误过滤的 0 tests）、callbacks **57/57**、prompt 路由 **18/18**、swarm **15/15**、spec **13/13**，均为 0 failed。
- `cargo check --lib` 0；`cargo clippy --lib` 0，本批留下的 `changed` 未使用告警已修正并复跑消失；本批具名文件 `rustfmt --check` 0、`git diff --check` 0。未运行全量 `cargo test --lib`，按分层验证执行上述定向回归。
- 绿灯门禁：`check:parity` 0、`check:event-coverage` 0（23 events）、`check:engine-i18n` 0（256/241）、`check:no-comments` 0（27 files）、`check:rust-module-deps` 0（recorded/found **423/423**，stale 0）、`scan:hardcoded:rust` 0（recorded/found **4012/4012**，new/stale 0）。
- 按行新增 `callbacks -> features`、`features -> swarm`、`server -> features`、`swarm -> features` 4 条依赖；按行删除 2 条已由 Markdown 资产取代的 stale，并将本批 5 条 `UNREVIEWED` 记录归类为 model-input/diagnostic。未运行任何 `--update`。
- `check:architecture` 仍因全工作区 `kimi-agent` fingerprint stale 为出口码 1，并有既有 i18n-runtime 软依赖 warning；按约束未刷新指纹，不把该全局门禁写成绿色。

**终态判定**：helloWorld 特性目录及其直接消费链已实现、接线并经真实工具与 HTTP 路径验证；本批无“随后”项、无半截状态、未提交 commit。其他并行工作区改动与 architecture 基线保持原状。

### 批次：operationLog 全量复核与真实接线（2026-11-13，终态）

本节取代早先“模型与磁盘存储”“git 视图服务”两节的中间判定。复核发现此前标为“等价”的实现仍有差异，且 service/snapshot 只有模块注册、没有消费方；本批已逐文件补齐并接通，不再保留“随后”“未接线”事项。

| v2 依据 | 引擎落点与终态 | 判定 |
| --- | --- | --- |
| `features/operationLog/operationLog.ts` | `tools/operation_log.rs`：视图、record/head/origin、retention、11 个错误码与 details；`refs` 排序比较与 v2 一致 | 等价 |
| `operationLogStore.ts` | `tools/operation_log_store.rs`：内容寻址、DAG/head 迁移、三级保留、不可变/原子写与锁。修正为 UUID nonce、仅小写 sha256 id、可选字段拒 `null`、缺失视图不再按 0 字节计算，锁重建使用 `create_new` | 等价 |
| `repositoryViewService.ts` | `tools/operation_log_repository.rs`：完整 capture/restore、`readFile(s)`/`readBlob(s)`、refs/packed-refs、缓存与锁。修正 8 层 symref、30s 锁超时、git 默认非零退出报错、unborn HEAD symbolic-ref、回滚失败保留 recovery 标记及目录身份判断 | 等价 |
| `operationService.ts` | `tools/operation_log_service.rs`：公开入口 flag 门、执行锁、外部变更导入、undo/redo/restore/restoreBeforeTurn、失败时反向恢复；恢复失败按 v2 传播反向恢复错误 | 等价 |
| `operationSnapshotStore.ts` | `tools/operation_log_snapshot.rs`：只读 snapshot 投影、path 防逃逸、agents/operations/views/files/blobs/changedFiles，写入口统一拒绝；after 缺失时仍写 `afterHash=""`、`afterSize=0` | 等价 |
| `flag.ts` | `operation_log` 默认关闭；专用 env → `[experimental]` → master env 的优先级与 v2 一致，并接入工具捕获、checkpoint、file-history、undo 与 shell 后台行为 | 等价 |
| `configSection.ts` | `config/mod.rs` 的 `[operationLog]` 与三个 `KIMI_CODE_OPERATION_LOG_*` 环境变量；正整数/非负整数及 JS safe-integer 边界逐项解析，配置登记到 `scripts/config-registry.json` | 等价 |
| `operationFeature.ts` | `NativeToolset` 持有共享 `OperationService`/repository 缓存，执行链按 v2 `repoMutation === "none"` 名单跳过；RAII 释放 shadow index，对应 feature 生命周期 | 等价（Rust 生命周期映射） |

**真实接线**：

1. `NativeToolset::execute_tool_streaming` 在实际工具执行前后捕获视图，origin 带 session/agent/turn/toolCall/toolName；无变化不记录，外部变更先导入。`execute_mutating` 统一走同一出口。
2. flag 开启时停用旧 checkpoint 与 SQLite 文件历史写入；`/file-history/changes`、`/file-history/content` 改读 operation 视图/blob，LCS 与二进制/缺失处理按 v2 `fileHistoryService`。`/undo` 在删历史前调用 `restoreBeforeTurn`，不再走另一套文件回滚。
3. Bash/PowerShell 在该 flag 下拒绝显式后台执行、禁止超时自动转后台，工具描述同步去掉后台指引；未开启时保持原路径。
4. `readFiles`/`readBlobs` 不再是留待后补的孤立方法：已用于真实 file-history HTTP 出口。

**验证（真实出口码）**：

- `cargo test --lib -- operation_log` → **29 passed / 0 failed**：26 条模型/存储/视图/service/snapshot 测试 + 配置边界 + NativeToolset Write→capture→undo + file-history/undo HTTP 全链路。
- `cargo test --lib tools::` → **1294 passed / 0 failed**；既有 undo 路由 **7/7**、file-history 路由 **5/5** 通过。Windows 下恢复文本按 Git `core.autocrlf` 的实际检出行为核对。
- `cargo check --lib` 0；`cargo clippy --lib` 0，且本批无告警；本批文件 `rustfmt --check` 0、`git diff --check` 0；5 个 operationLog 新模块无任何源码注释。
- 绿灯门禁：`check:config-registry`（37 sections）、`check:parity`、`check:engine-i18n`（256/241）、`check:locale-orphans`（680/680）、`check:no-comments`（27 files）。
- 本批手工逐行新增 37 条硬编码记录、删除 1 条 stale，未运行 `--update`。最终扫描为 recorded 3947 / found 4013、**stale 0**，剩余 66 条全部位于 helloWorld、promptOptimizer、spec/subagent 等其他在途文件。
- 全局基线未伪称全绿：`check:rust-module-deps` 为 recorded 415 / found 419，剩余 4 条全部来自其他在途特性；`scan:hardcoded:rust` 因上述其他批次 66 条仍为出口码 1。operationLog 自身无新增模块依赖、无未登记字面量、无 stale。

**终态判定**：operationLog 特性目录 8 个文件及其直接消费链均已实现、接线并经真实路径验证；本批无“随后”项、无半截状态、未提交 commit。其他批次的基线失败单独保留，不用扩大本批改动或刷新基线的方式掩盖。

### 收口：operationLog 旧容器删除与逻辑归属（2026-11-15）

本节记录 operationLog 四文件 + 核心文件的清扫批次，不改写上方 2026-11-13 终态记录；旧容器删除后以本节为准。

| 项 | 结果 | 去向 / 备注 |
| --- | --- | --- |
| `tools/operation_log_store.rs` | 已删除 | 实现已在 `features/operationLog/operationLogStore.rs`；引用改接后删除 |
| `tools/operation_log_snapshot.rs` | 已删除 | 只读 snapshot 投影在 `features/operationLog/operationSnapshotStore.rs`；`file_history_*` 迁出 |
| `tools/operation_log_repository.rs` | 已删除 | 实现已在 `features/operationLog/repositoryViewService.rs` |
| `tools/operation_log_service.rs` | 已删除 | 实现已在 `features/operationLog/operationService.rs`；`operation_log_captures_tool` 移出 |
| `tools/operation_log.rs` | 已删除 | 核心类型/常量/错误/4 个内联测试搬入 `features/operationLog/operationLog.rs`；移除 re-export 层 |

**迁出逻辑**：

- `file_history_changes` / `file_history_all_changes` / `file_history_content` → `features/fileHistory/file_history_service.rs` 的 `operation_file_history_changes` / `operation_file_history_all_changes` / `operation_file_history_content`；HTTP 路由与 `AgentFileHistoryService` 已改接。
- `operation_log_captures_tool` → `tools/dispatch.rs`，名单与归一化逻辑原样保留；`CaptureContext.repo_mutation` 暂统一 `None`。
- `AgentOperationService` 三处适配：`repository_views` 使用 `Arc<StdMutex<HashMap<...>>>`、`#[derive(Clone)]`、`pub(crate) with_identity`；待 `ToolSet` 拆分为 per-agent 后回归 v2 形状。

**验证**：`cargo check -p kimi-agent` 仍为 exit 101，198 条错误均来自其他在途批次；operationLog/fileHistory/dispatch/toolset/route 相关文件无报错；被改文件 `rustfmt --edition 2024` 解析通过。被删 22 个内联测试与待接线项见 `porting-backlog.md` 的「待收口项」。

 --- | --- | --- | --- |
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
| `permissionMode/permissionMode.ts` / `permissionModeOps.ts` / `configSection.ts` / `permissionModeService.ts` | `permission/mod.rs` + `agent/permission_mode/*` + `session/sqlite_store.rs` + `session/wire_journal.rs` + `server/engine.rs` + `napi_bindings/session.rs::session_set_permission_mode` + 宿主 | 等价 | 模式持久化/状态事件此前由宿主发布，2026-11-12 起引擎也写 durable `permission.set_mode`：`record_mode`/`recorded_mode` 走 sqlite wire_events，journal 列为 durable 记录，`resolve_permission_mode` 以记录优先，napi 切换经 SessionEntry 的 store 复用同一条链；`defaultPermissionMode` 归宿主 |
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

### 批次：tower artifact 任务类型（2026-11-13）

用户指令「优先把代码搬过来，其次再是接线」，据此选定 v2 tower 的 **artifact 任务类型**：引擎此前只有 `build` / `survey` 两种 kind，artifact 整块缺失（类型、complete 的 key_artifacts 校验与验收报告、review 的 acceptance_tip 绑定、merge gate 的验收绑定、rebase 冲突文案、worker/reviewer 简报）。参考实现为 `D:\kimi\kimi-code` 工作区 `packages/agent-core-v2/src/features/tower/`。

| v2 依据 | Rust 落点 | 判定 |
| --- | --- | --- |
| `protocol/types.ts:34`（`TowerMissionKind` 第三态）、`:56-57`（`completionTip` / `keyArtifacts`） | `tools/tower/types.rs` 新增 `TowerMissionKind::Artifact`（`as_str` → `artifact`）与 `TowerMission.completion_tip` / `key_artifacts`（camelCase，skip_serializing_if） | 等价 |
| `protocol/store.ts:1513-1524`（completed 时记录/清除 completion tip 与 key artifacts） | `store.rs::update_mission_in_state` 在同分支写入；`MissionUpdateOptions` 增 `completion_tip` / `completion_key_artifacts` | 等价 |
| `protocol/store.ts:1579-1590`（artifact 的缺分支/零差异文案） | `store.rs::assert_completable` 按 kind 选择 `an artifact mission must produce committed deliverables / a committed diff` 措辞 | 等价 |
| `protocol/store.ts:1688-1835`（`complete` 的 key_artifacts 归一、三条校验、tip 计算、验收报告、sameCompletion） | `store/complete.rs`：`normalize_artifact_paths`（`posix.normalize` 语义 + 绝对/逃逸/空拒绝 + 保序去重）、`branch_exists` → `branch_tip` → `diff_base` → `diff_name_only`、`cat-file -t` 逐路径验 blobs、`artifact_acceptance_report`、`same_artifact_paths` 门控通知复用 | 等价 |
| `protocol/store.ts:1905-1913`（状态丢失时 key artifacts 前缀） | `recovered_completion_input(report, key_artifacts)` 拼 `Key artifacts:\n- …\n\n{report}` | 等价 |
| `protocol/store.ts:2227-2251`（review 的 artifact 验收四守卫） | `store.rs::submit_review_prepared_locked`：无 completion tip / 缺 acceptance_tip / acceptance_tip 不等于记录 tip / reviewed_commit 不等于记录 tip，逐条逐字拒绝 | 等价 |
| `protocol/missionGate.ts:183-247`（artifact 的 stale-base / no-review / reviewer=tower / hold / not-clean / not-approved / tip-moved / completion-tip-mismatch） | `mission_gate.rs` 增 `REASON_COMPLETION_TIP_MISMATCH`，八个分支按 `is_artifact` 切分文案 | 等价（文案按本引擎既有「merge blocked:」缩写风格，与 build/survey 一致） |
| `protocol/store.ts:3069-3070`（artifact rebase 冲突 blocker 文案） | `store/rebase.rs` 按 kind 切分冲突 blocker | 等价 |
| `tools/plan/plan.ts`（kind 枚举与 scope 措辞）、`tools/complete/complete.ts`、`tools/review/review.ts` 的 schema 描述 | `tools/tower/mod.rs` 的 `TowerComplete` / `TowerReview` / `TowerPlan` 三个 input_schema 同步 | 等价 |
| `tools/spawn/spawnTool.ts:460-620`（worker 的 artifact 交付步骤与 reviewer 的 advisory 说明） | `build_worker_prompt` / `build_reviewer_prompt` 按 kind 切分（「what to deliver」、commit-every-artifact、key_artifacts 参数、acceptance 绑定最后一 tip、review advisory） | 等价（沿用本引擎既有缩略简报风格） |
| `tools/{complete,review,plan,merge,rebase}/*.md` 的 artifact 段落 | `tools/tower/descriptions/*.md` 同步六处描述 | 等价 |

**原生替代登记**：

- v2 的 `posix.normalize` → 自实现分段归一（`""`/`"."` 折叠、`..` 弹栈并标记逃逸、前导 `/` 拒绝），因为引擎不引 posix crate；语义与 v2 的 reject 条件逐条对齐（空、含 `\0\r\n`、绝对、`..` 逃逸）。
- v2 的 `picomatch.isMatch` 作用域匹配 → 复用本引擎既有的 `globset::Glob` + `trim_end_matches('*')` 前缀回退（与 `mission_gate.rs` / `store.rs` 的 out-of-scope 判定同一实现）。
- v2 的 `tryGit(...) === 'blob'` → `git(cat-file -t tip:path)` 失败按空串处理（`unwrap_or_default`），与 `tryGit` 返回 `null` 同义。

**验证**：

- 新增 8 条用例（`store/tests.rs`）：路径归一/去重/七种拒绝、验收报告逐字、tip 记录与通知去重、三条 key_artifacts 校验、净差异外路径、review 四守卫中的三条、merge gate 的两条 artifact 阻断与 tip 漂移。`cargo test --lib artifact_` → **8 passed / 0 failed**。
- `cargo test --lib tower::` → **130 passed / 0 failed**（格式化后复跑同样 130/0）。
- `cargo clippy --lib` 与 `rustfmt --edition 2024 --config skip_children=true --check` 对 11 个触碰文件均 0 命中。
- 门禁：`check:parity` 0（tools 86）、`check:rust-module-deps` 415/415、`check:engine-i18n` 256/243、`scan:hardcoded:rust` 3856/3856（`-- --update` 前 45 new / 4 stale，更新后 0/0；差异仅落在本批 5 个 tower 文件 +45/-4，无越界）。
- `check:architecture` 与 `check:normify` 仍红，均为**在途批次既有状态**：architecture 仅 kimi-agent 指纹 stale（stored=`556ae7f833644f98`、current=`0212f9ca8c06ba68`），normify 仅 `engine.napi.bindings` / `engine.napi` 指纹漂移；本批未新增模块、未改 napi，故未执行 update。

**本批未宣称完成的部分**：

- `tools/spawn/spawnTool.ts` 的四条文案/守卫差异中，artifact 简报已搬，但既有 build/survey 的缩略差异不在本批。
- `tools/review/reviewTool.ts` 的 notified 名单与 next 指引仍未移植（manifest 记为 partial）。
- `features/tower/tower.ts` 的 enter/exit 失败分类与文案、`towerOps.ts` 的 durable 事件仍未接线（接线类，按用户指令后续再做）。
- artifact 的 `cat-file` 逐路径校验在正常流程中不可达（净差异内的路径必为 blob），仅作防御保留，未单测。

### 批次：tower review 结果面（notified / next / 自动运行守卫）（2026-11-13）

续 artifact 批次，补 v2 `TowerReview` 工具的结果面。此前 Rust 只输出 `review submitted: <file>` 与通知计数，v2 的收件人名单、`next:` 指引与自动运行守卫缺失。

| v2 依据 | Rust 落点 | 判定 |
| --- | --- | --- |
| `tools/review/reviewTool.ts:39-53`（artifact 验收在 auto/非交互下拒绝记录） | `tools/tower/mod.rs::execute_tower_review` 新增 `non_interactive` / `permission_mode` 两个入参（由 `tools/dispatch.rs` 从 toolset 既有字段传入），`caller == tower && artifact && clean && merge && (non_interactive \|\| Auto)` 时返回 isError | 等价 |
| `tools/review/reviewTool.ts:86-92`（`notified:` 收件人名单） | 由 `result.notifications` 的 `item.to` 拼接，空集为 `(none)` | 等价 |
| `tools/review/reviewTool.ts:93-101`（artifact 的四个 `next:` 分支） | 按 `status`/`merge`/`caller`/`review.mission` 逐分支还原；评审员提交 clean+merge 走「still waiting for mission-matching explicit human acceptance」 | 等价 |
| `tools/review/reviewTool.ts:102-131`（build 的三个 `next:` 分支 + hold/fix-then-merge 文案） | 同上逐字还原 | 等价 |
| `protocol/store.ts:180-186`（`TowerReviewResult.notificationError` / `activityLogError`） | `TowerReviewResult` 增 `activity_log_errors`；`submit_review_prepared_locked` 收集 `mission.rework` 日志失败与各通知的 `activity_log_error`，按 ` \| ` 连接输出 | 等价 |
| `protocol/store.ts:2388-2400`（isError 由两个 error 字段决定） | `has_error = !notification_errors.is_empty() \|\| !activity_log_errors.is_empty()` | 等价 |

**原生替代登记**：v2 的 `bootstrap.args.nonInteractive` + `permissionMode.mode` → 引擎 toolset 既有的 `non_interactive` 与 `permission_mode` 字段（`exit_plan_mode` / `spec_mode` 已在用同一来源），未新增跨层通道。

**验证**：

- 新增 3 条用例（`tools/tower/mod.rs` 测试模块）：`review_output_lists_recipients_and_next_guidance` 覆盖 build 的 merge-ready / hold / fix-then-merge / not-merge-ready 四条 `next:` 与 `notified: tower`；`artifact_acceptance_is_refused_in_an_automatic_run` 覆盖 non_interactive 与 Auto 两种信号；`artifact_review_next_distinguishes_the_tower_from_a_reviewer` 覆盖评审员与 tower 两条 artifact `next:` 及 `notified:` 差异。`cargo test --lib tools::tower::tests::` → **13 passed / 0 failed**。
- `cargo test --lib tower::` → **133 passed / 0 failed**（含上一批 130 条）。
- `rustfmt --check` 0；`cargo clippy --lib` 对 tower 无新告警（仅既有 `on_tower_mode_enter` dead-code）。
- 门禁：`check:parity` 0（tools 86）、`scan:hardcoded:rust` 3870/3870（`--update` 前 15 new / 1 stale，更新后 0/0；差异仅 `tower/mod.rs` +14、`tower/store.rs` +1、删除旧输出字面量 1 条）。
- `check:architecture` / `check:normify` 仍为在途批次既有红（未新增模块、未改 napi）。

**本批未宣称完成的部分**：`reviewTool.ts` 的 `delivery` 追加段（v2 把 `deliverTowerMessage` 的投递回执拼在末尾）在引擎由 wake runtime 承担，未逐字还原；`review.ts` 的 notified 名单去重顺序依赖 v2 的收件人解析，引擎已有等价实现。

### 批次：cron 上限校验顺序（2026-11-13）

续 tower 批次期间的用户指令「优先把代码搬过来，其次再是接线」。本刀只含一处引擎内行为差异。

| v2 依据 | Rust 落点 | 判定 |
| --- | --- | --- |
| `tools/cron-create/cronCreateTool.ts:62-67`（cap 检查在 prompt 字节数之前），`:120-127`（execute 阶段再查一次） | `tools/cron_tools.rs`：`validate_create` 增 `scheduled` 入参，把 cap 检查插到 `has_fire_within_years` 之后、`validate_prompt_bytes` 之前；`execute_cron_create` 把 `state_read`（计数）提前到 `validate_create` 之前，写前的一次 cap 复查保留 | 等价 |

**验证**：

- 新增 `test_validate_create_reports_the_session_cap_before_the_prompt_size`：在到达上限且 prompt 超 8 KiB 时，返回值是 cap 文案而不是字节数文案。`cargo test --lib tools::cron_tools` → **28 passed / 0 failed**。
- `rustfmt --check` 0；`cargo clippy --lib` 对 `cron_tools` 无告警。
- 门禁：`scan:hardcoded:rust` 3870/3870（无新增/失效条目，被移动的 `CRON_JOB_CAP_MESSAGE` 是同文件内既有常量）、`check:parity` 0。
- `check:architecture` / `check:normify` 仍为在途批次既有红。

**顺带核实的两条过时 manifest 判定**（本批读码得出，未改 manifest，因为它们是机器行、会被再生成覆盖）：

- `features/cron/configSection.ts` 记「生效配置只接了 3/7 个键」——实际 7 个键在 `src/features/cron/cronService.rs` 全部有消费点（`debug`/`clock` → `resolve_clock_sources`，`no_jitter` → jitter，`no_stale` → stale 判定，`disabled` → tick 与 `is_disabled`，`manual_tick` → 轮询分支，`poll_interval_ms` → 定时器间隔）。
- `features/lsp/lspConnection.ts` 记「服务端→客户端请求的应答未移植」——实际 `src/tools/lsp_tool.rs::answer_server_request` 已按 `workspace/applyEdit`（-32601）、`workspace/configuration`（null 数组）、其余（result null）分别应答，比 v2 的默认 null 处理器更完整；仅 AbortSignal 取消由超时替代。

**本批未宣称完成的部分**：cron 工具侧的 `cron_scheduled` / `cron_deleted` 遥测仍未发——`emit_scheduled` / `emit_deleted` 存在于 `features/cron/cronService.rs` 但无工具侧调用方，接它需要把 cron actor 服务穿进 toolset，属接线类，按用户指令留后。

### 缺口核查：在途重构树、过时判定与真正的特性级缺口（2026-11-13）

用户指令「优先移植功能、其次接线」下做的一次系统核查。方法：对 manifest 的每条红项回到引擎源码验证，而不是采信机器判定。结论分三类。

**一、在途重构树（状态记录，非缺陷）**

| 项 | 证据 |
| --- | --- |
| `src/agent/**`、`src/app/**`、`src/features/**`、`src/human/**`、`src/os/**`、`src/persistence/**`、`src/tool/**` | 这些目录在 `git ls-files` 里几乎全未追踪（`src/agent/` 仅 4 个文件是 `AM`），`src/lib.rs` 的工作区改动新增了 `mod agent` / `pub mod app` / `pub mod features` / `pub mod human` / `pub mod os` / `pub mod persistence` / `pub mod tool`。这是在途批次正在把 v2 的目录布局（`agent/tool_*`、`tool/`、`features/git_memory` 等）搬进引擎，与既有的扁平模块（`src/tools/*`、`src/goal/*`）并存。 |

因此**不能**把这些文件判为「仓库死代码」：它们是未提交的重构在途件。就 `src/features/` 内部而言，目前只有 `associativeMemory` 被既有代码引用（`src/tools/dispatch.rs`、`memory_graph_tool.rs` 等 10 处），`cron` / `dateChange` / `goal` / `notify` / `reminder` / `snapshot` / `todo` 尚未被任何既有代码引用——这是迁移中途的正常状态，但值得记录，以免被误判为「实现了没接」。

其中一条对后续接线有用的事实：`src/features/goal/goalService.rs` 已经实现了 v2 的两条陈旧守卫 `is_goal_tool_target` / `is_objective_edited_since_turn_start`（含 `goal_turn_targets` / `turn_objective_revisions` / `objective_revision` 状态），但既有工具 `src/tools/goal_tools.rs::execute_update_goal` 没有调用它们——这正是 manifest 里 `features/goal/tools/update-goal/updateGoalTool.ts` 那两条「服务侧守卫未移植」的真正落点。

**二、判定已过时的 manifest 红项（读码核实，未改 manifest——它们是机器行，再生成会覆盖）**

| manifest 行 | 原判定 | 实际 |
| --- | --- | --- |
| `features/cron/configSection.ts` | 只接了 3/7 个键 | 7 个键在 `cronService.rs` 全有消费点（debug/clock→`resolve_clock_sources`、no_jitter→jitter、no_stale→stale 判定、disabled→tick 与 `is_disabled`、manual_tick→轮询分支、poll_interval_ms→定时器） |
| `features/lsp/lspConnection.ts` | 服务端→客户端请求应答未移植 | `src/tools/lsp_tool.rs::answer_server_request` 已按 `workspace/applyEdit`(-32601)、`workspace/configuration`(null 数组)、默认(null) 分别应答，比 v2 的默认 null 处理器更完整；仅 AbortSignal 取消由超时替代 |
| `features/btw/btwService.ts` | 名未移植 | `src/subagent/btw.rs` 已含 `TOOL_CALL_DISABLED_MESSAGE` / `SIDE_QUESTION_SYSTEM_REMINDER` / `check_btw_tool_denial`，`main.rs` 有 `session/start_btw` / `btw_prompt` / `btw_cancel` |
| `injection/renderedLines.ts` | NA（本 fork 无该特性） | `src/agent/injection/rendered_lines.rs` 已逐行移植（sha256 前 16 字符指纹、1024 行 / 32 会话上限、全局 `LazyLock`），`associativeMemory/service.rs` 在用 |
| `features/codeRuntime/memoryGuard.ts` | 守护抽象与 Windows 分支未移植 | 架构不同而非缺失：`src/workflow/code_runtime.rs` 用 rquickjs 进程内沙箱 + `set_memory_limit`（512 MiB 同值），v2 是子进程 + 作业对象/`/proc`/`ps`；引擎没有子进程可守护，该抽象无对应物 |
| `features/gitMemory/gitMemoryEntries.ts` | 缺 alreadyRendered 会话去重 | 提示词每会话构建一次（`server/engine.rs::session_spec` 只在 skills 路径被调用，回合复用缓存的 `PipelineSpec.system_prompt`），不存在逐回合重复注入；`RenderState` 服务于 repo_map 工具自身的渲染去重 |
| `features/skill/session/skillCatalogService.ts` | 缺增量 remerge | 引擎全量重扫，结果等价，仅耗时不同 |

**三、真正的特性级缺口（整块未移植，均需先定方案）**

| 特性 | v2 规模 | 引擎现状 | 缺口性质 |
| --- | --- | --- | --- |
| `features/helloWorld` | 446 行（service 255 / ops 121 / types 51 / feature 19）| 零痕迹 | 新状态域 + 新 durable 事件 + injection + 模式互斥 + telemetry；v2 的事件在 `helloWorldOps.ts` 有对应物，属移植非发明，但要动 `STATE_DOMAINS`（现 7 个） |
| `features/operationLog` | 8 文件（含 snapshotStore / repositoryViewService）| 零痕迹 | 整块操作日志与仓库视图 |
| `features/promptOptimizer` | 7 文件 | 零痕迹 | 整块提示词优化 |
| `features/debugEvents` | 162 行 | 零痕迹 | v2 ledger  introspection 的开发面，价值低 |
| `session/subagent/backend` | 9 文件 | 只有拒绝桩（`agent_tool/execute.rs::BACKENDS_UNAVAILABLE`）| ACP / Claude Code / Codex 子代理后端，整块 |

**下一步建议**：以上都不是单刀能收口的规模。建议按用户优先级先做一个：`helloWorld`（上游完整特性、边界清楚）或 `operationLog`。动手前需要一份方案（新增状态域、durable 事件登记到 `scripts/upstream-v2-delta-allowlist.json`、injection 文案、模式互斥交互），并按仓库规则取得授权。死模块的处置同样等裁定。

### 批次：移除两处 fork 自增（TodoList `updates` / Workflow `list`）（2026-11-13）

承上一节的核查。manifest 里仅剩两条「fork 有、v2 无」的行为差异，均无出处标注，按仓库政策属缺陷。用户裁定「移除以对齐 v2」。

| v2 依据 | 引擎改动 | 判定 |
| --- | --- | --- |
| `features/todo/tools/todo-list/todo-list.ts:39-56`（输入只有 `todos?`） | `src/tools/todo_list.rs`：删掉 `has_updates` 分支、`update_todo_list`、`read_current_items`（合计 117 行）、schema 的 `updates` 属性（42 行）、描述里的「Keep updates cheap」段与两条 `updates` 用法、4 条 `updates` 测试（105 行） | 等价 |
| `features/todo/tools/todo-list/todo-list.md:22-24`（只有 `todos` 三种用法） | 描述的「How to use」改为 v2 原文的三条 | 等价（描述其余段落是引擎既有的额外指引，不在本次裁定范围） |
| `app/workflow/tools/workflow.ts:21`（`WorkflowOperationSchema` 只有 run/status/wait/cancel） | `src/tools/workflow.rs`：enum 去掉 `list`、删 `handle_list`、删 `list_includes_the_builtins` 测试；`src/tools/tests.rs` 的 `workflow_tool_lists_without_a_subagent_runtime` 改为 `workflow_tool_run_requires_a_subagent_runtime`（只保留「无 subagent runtime 时 run 被拒」） | 等价 |

**原生替代登记**：无新增。`WorkflowService::list_builtins` / `registry::list_builtins` 保留为 `pub` API（`pub fn` 不产生 dead-code 告警，且属服务库表面，不是模型可见面）。

**验证**：

- `cargo test --lib --offline -- tools::todo_list tools::workflow` → **16 passed / 0 failed**；`cargo test --lib tools::` → **1262 passed / 0 failed**（唯一失败 `workflow_tool_lists_without_a_subagent_runtime` 已按 v2 行为改写）。
- `rustfmt --check` 0；`cargo clippy --lib` 对两个文件无告警（不再使用 `arg_error` / `arg_error_text` 后已从未用导入中移除）。
- 门禁：`check:parity` 0（tools 86）、`check:engine-i18n` 256/241、`check:locale-orphans` 680/680、`check:locale-keys`、`check:config-registry`、`check:event-coverage`、`check:rust-module-deps` 415/415 全绿；`scan:hardcoded:rust` 3852/3852（`--update` 前 18 条 stale，全部位于本批两个文件，更新后 0/0）。
- `check:architecture` / `check:normify` 仍为在途批次既有红。

**未宣称完成**：`features/sessionQuery/toolInput.ts`（校验文案为引擎自撰）与 `features/tower/tools/send/sendTool.ts`（多一条 fork 投递提示）两条 manifest 记为「引擎自撰/多一条」但未标为缺陷——它们是文案差异而非 schema 差异，本次未动。

### 批次：TowerSpawn 的名称与任务守卫（2026-11-13）

承上一节对 tower 剩余 partial 的逐条复核。多数判定已过时（`initTool.ts` 的「缺占用保护」实际由 `store.rs::adopt_foreign_roster` 的会话锁守卫覆盖，且比 v2 的「注册表层」判定更强；`plan.ts`、`complete.ts`、`review.ts` 的 artifact 相关缺口已在前面批次闭合）。本批补最后四条真实差异。

| v2 依据 | 引擎改动 | 判定 |
| --- | --- | --- |
| `tools/spawn/spawnTool.ts:150-162`（名称非空且无首尾空白；保留名 `tower` / `all`） | `tools/tower/mod.rs::execute_tower_spawn` 在既有代理存在性检查之前插入两条守卫，文案逐字 | 等价 |
| `tools/spawn/spawnTool.ts:186`（`unknown mission "…" — known missions: …`，空集为 `(none planned yet)`） | worker 分支的未知任务文案改为带已知列表（与 `TowerMission` 工具既有写法一致） | 等价 |
| `tools/spawn/spawnTool.ts:192`（`mission … is … — terminal missions cannot spawn workers; no worktree or agent was created`） | worker 分支的终态任务文案逐字对齐；reviewer 分支的自有守卫改为同构措辞（v2 无此守卫，属引擎既有新增） | 等价 |

**验证**：

- 新增 `spawn_refuses_blank_reserved_and_unknown_mission_names`：覆盖空/带空白名、两个保留名、未知任务带已知列表；并给既有 `spawn_refuses_a_terminal_mission_before_requesting_runtime` 补 `terminal missions cannot spawn workers` 断言。`cargo test --lib tools::tower::tests` → **14 passed / 0 failed**；`cargo test --lib tower::` → **134 passed / 0 failed**。
- `rustfmt --check` 0；`cargo clippy --lib` 对 `tower/mod.rs` 无告警。
- 门禁：`scan:hardcoded:rust` 3854/3854（`--update` 前 +5 new / −3 stale，全部位于 `tower/mod.rs`，更新后 0/0）、`check:parity` 0（tools 86）。
- `check:architecture` / `check:normify` 仍为在途批次既有红（见下一节）。

### 批次：TowerTeardown 的占用守卫与输出文案（2026-11-13）

| v2 依据 | 引擎改动 | 判定 |
| --- | --- | --- |
| `tools/teardown/teardownTool.ts:48-56`（工作区被其它活跃会话拥有时拒绝拆除） | `tools/tower/mod.rs::execute_tower_teardown` 在状态锁之后、live agent 采集之前插入守卫：读 `state.session_id`，若不属于当前会话且该会话在 `SESSION_LOCKS` 中被持有（本进程或另一进程），返回 v2 文案 | 等价（锁判定比 v2 的注册表判定多覆盖跨进程情形） |
| `tools/teardown/teardownTool.ts:72-73`（dry-run 标题） | 输出首行改为 `tower teardown (dry run — nothing was changed):` / `tower teardown:` 二选一 | 等价 |
| `tools/teardown/teardownTool.ts:78-80`（收尾说明） | 末行改为 v2 原文（含 `/tower off` 与「remove it by hand only if you are sure」） | 等价 |

**验证**：

- 新增 `teardown_refuses_a_workspace_owned_by_a_live_session`（用 `SESSION_LOCKS.hold_guard` 占住前会话，断言拒绝文案）。`cargo test --lib tools::tower::tests` → **15 passed / 0 failed**；`cargo test --lib tower::` → **135 passed / 0 failed**。
- `rustfmt --check` 0；`cargo clippy --lib` 对 `tower/mod.rs` 无告警。
- 门禁：`scan:hardcoded:rust` 3856/3856（`--update` 前 +3 new / −1 stale，全部位于 `tower/mod.rs`）、`check:parity` 0、`check:engine-i18n` 256/241。
- 描述资产 `descriptions/teardown.md` 与 v2 逐字相同，无需改动。
- `check:architecture` / `check:normify` 仍为在途批次既有红。

**未宣称完成**：dry-run 输出只做了代码改动，未单测——`store/tests.rs::teardown_supports_dry_run_live_agent_and_exclude` 覆盖 dry-run 的磁盘/状态语义，而工具层输出是纯字符串拼接；工具层 dry-run 端到端需要先 `add_worktree` 造出真实 worktree（本次尝试后因 git 前置条件成本超出断言价值而撤回该测试）。

### 批次：cron 工具遥测接线（cron_scheduled / cron_deleted）（2026-11-13）

用户指令的「其次接线」部分。前一批已确认 `emit_scheduled` / `emit_deleted` 只存在于在途的 `src/features/cron/cronService.rs`、无工具侧调用方。复核发现接线不需要该 actor 服务：`HostCallbacks` 自带 `telemetry(&self, event)` 宿主遥测出口（`callbacks.rs:270`），既有 `compaction_finished` 等事件就走它。

| v2 依据 | 引擎改动 | 判定 |
| --- | --- | --- |
| `features/cron/cronService.ts:629-635`（`emitScheduled(task, agentId)`，属性 `{recurring, agent_id}`） | `execute_cron_create` 增 `agent_id` 入参（由 dispatch 传 `effective_caller_agent_id()`），在 `state_write` 成功后发 `{"event":"cron_scheduled","recurring":…,"agent_id":…}` | 等价 |
| `features/cron/cronService.ts:637-641`（`emitDeleted(taskId, agentId)`，属性 `{task_id, agent_id}`） | `execute_cron_delete` 同样增参，成功后发 `cron_deleted`；状态存储在任务不存在时已返回 `No cron job with id …`（-32604），与 v2 的 `removed.length === 0` 分支同义，故只在成功时发 | 等价 |

**验证**：

- `ScriptedCallbacks` 增 `telemetry_received` 捕获（不改 `scripted()` 三元组签名，新增 `telemetry_events()` 读方法），13 处工具调用点补 `"main"` 入参。
- 断言：create 成功发 1 条 `cron_scheduled`（recurring/agent_id 正确）；delete 成功发 1 条 `cron_deleted`（task_id/agent_id 正确）；达上限被拒时不发。`cargo test --lib -- tools::cron_tools` → **28 passed / 0 failed**；`cargo test --lib tools::` → **1264 passed / 0 failed**。
- `rustfmt --check` 0；`cargo clippy --lib` 对两个文件无告警。
- 门禁：`check:parity` 0（tools 86）、`check:engine-i18n` 256/241、`scan:hardcoded:rust` 3856/3856（新字面量位于 `serde_json::json!` 宏内，扫描器按既有行为不计入，与 `compaction_finished` 一致）。
- `check:architecture` / `check:normify` 仍为在途批次既有红。

**未宣称完成**：`cron_fired` / `cron_missed` 的触发路径在引擎里另有出处（回合驱动与 stale 清理），本批只接工具侧的 scheduled/deleted 两条；`emitScheduled` 在 v2 还带 `task.id`，引擎只发 `recurring`（v2 的 `CronScheduledEvent` 只有这两个属性，一致）。

### 补记：`toShellPath` 方向经核实为无消费方（2026-11-13）

复核 `_base/execEnv/shellPathBridge.ts` 的「native→shell 方向未移植」判定时确认：v2 唯一的消费方是 `agent/tools/os/bash/bashTool.ts:155`——它把 `effectiveCwd` 转成 MSYS 形式后拼进 `cd ${shellQuote(shellCwd)} && ${command}`，同时仍用原生 cwd spawn。引擎的 `tools/bash_exec.rs` 不注入 `cd`，直接用 `Command::current_dir(&work_dir)`（`bash_exec.rs:95,293`），由操作系统解析原生路径，Git Bash 启动后 PWD 即 MSYS 形式。因此 `toShellPath`（`toMsysPath`）在引擎里**没有消费方**，移植它会得到死代码。

> 这是一条判定为「部分」但实际应归「NA（架构不同）」的条目：引擎保留了 bridge 的 `to_native_path`（shell→native，`tools/file_ops.rs:88` 在用），只裁掉了反向。若将来引擎改为注入 `cd`，再补 `to_msys_path` 及其单测。

### 补记：`todo.updated` 事件为架构差异（2026-11-13）

`features/todo/todoService.ts` 的 `publish` 在 v2 会发 `todo.updated`（`{agentId, todos, rev}`，observable），SDK 侧有 `todo-updated-event.test.ts`。本仓的 TUI 不靠该事件：`apps/kimi-code/src/tui/controllers/session-event-handler.ts:547` 读 `host.state.todoPanel.getTodos()`、`:923` 读工具调用参数，宿主状态由 `state_write` 域驱动。引擎、`packages/protocol`、`packages/node-sdk` 三处都没有 `todo.updated`。manifest 记的「publish 死代码」与此一致——不是缺失，是通道不同。

### 补记：helloWorld 是「需要宿主入口」的特性，不是单刀（2026-11-13）

动手前把 v2 的入口链读了一遍，结论：**helloWorld 在 v2 里没有独立的宿主开关**——目录下没有 `flag.ts`（manifest 那一行是过时机器行，参考检出里根本没有该文件），也没有任何 `switchTo('hello-world')` 或 `enter('manual')` 的调用方在 `features/helloWorld/` 之外。唯一的进入方式是 `modeMutexService.ts` 的 `switchTo` → `enter('hello-world')` → `helloWorld.enter('manual')`，而 `switchTo` 由宿主（TUI 的模式切换）驱动。

因此完整移植 helloWorld 需要同时动三处：引擎（2 个状态域 + 3 个 durable 事件 + 7 个 stage 脚本 + injection provider + 模式互斥）、`apps/kimi-code` 的宿主入口（一个切模式的命令/开关）、以及可能的 `packages/protocol`。这是**跨层特性**，按仓库规则需要先出方案并取得授权，不是「搬代码」能单刀收口的规模。

规模估计（读码得出）：`helloWorld.ts` 类型 51 行、`helloWorldService.ts` 255 行（7 条 evidence 规则 + 状态机）、`helloWorldOps.ts` 121 行、injection 约 100 行 + 7 个 stage 脚本、模式互斥 5 条冲突边、telemetry 2 条、测试若干。

**其它未移植特性同样需要先定方案**：`operationLog`（8 文件，自带 snapshotStore / repositoryViewService，无宿主入口依赖，可能是下一个最自包含的）、`promptOptimizer`（7 文件）、`debugEvents`（162 行，开发面）、`session/subagent/backend`（9 文件，引擎只有拒绝桩）。

补一条同类判定：`promptOptimizer` 也是**宿主面特性**——`optimize()` 的唯一调用方是 `kap-server/src/routes/sessions.ts:1097`（session API），flag 描述明写「reaching it from the TUI shortcut and the session API」。引擎侧的对应落点是 `server/route_sessions.rs`，但 `PROMPT_OPTIMIZER_AGENT_ORIGIN`（`session/subagent/runAgentTurn.ts:30`）也在子代理回合路径上，所以同样是跨层。`operationLog` 的 8 个文件里没有 flag、没有宿主入口调用（只有 DI feature + service），是这几个里唯一可能纯引擎闭合的，但 2232 行的量级（`repositoryViewService.ts` 868、`operationLogStore.ts` 600）本身就是多刀工程。

### 补记：tower 模式的生命周期事件与 takeover（2026-11-13）

`features/tower/towerService.ts` 的「生命周期编排未接线」拆开后是三件事，性质各不相同：

| v2 行为 | 依据 | 引擎现状 |
| --- | --- | --- |
| `tower_mode.enter` / `tower_mode.exit` 两个 durable 事件 | `towerService.ts:420-426,519-526` 经 `TowerModeEnter` / `TowerModeExit` | **完全没有**——引擎的 tower 进入（`execute_tower_init`）与退出（`execute_tower_teardown`）都不发模式事件（`grep '"type": "tower'` 在 tools/server 下零命中） |
| `tower_mode_enter` / `tower_mode_exit` 两条 telemetry（含 `outcome`/`reason`） | 同上，`track2(...)` | 没有 |
| 跨会话 takeover：新会话调 owner 会话的 `tower.exit('takeover')` 释放工作区 | `towerService.ts:443-447` | 引擎的 takeover 是**会话锁**层面的（`session/lock.rs:241` + `POST /api/v1/sessions/{id}/takeover`），不触碰 tower 工作区；`store.rs::adopt_foreign_roster` 只在 owner 未持锁时静态收养名册 |

前两条与既有的 `swarm_mode.enter/exit`、`spec_mode.exit` 同构，属于可闭合的接线（但 v2 的 owner 侧退出发生在**另一个会话**的事件总线上，引擎里没有跨会话事件通道，只能由 REST takeover 路由写 owner 的 tower 域——这层需要设计）。第三条同理。

> 这条与 `plan_mode.*` 的补记合起来看：引擎的模式事件是**逐模式补齐**的（swarm 全、spec 半、tower/plan 无），不是统一缺失。补的时候要逐模式定「谁在哪个会话上发」，不能一律在工具侧发。

### 批次：tower 模式事件与遥测（本会话侧）（2026-11-13）

按上一节的分解，先落地**本会话内**的两条（跨会话 takeover 与状态载荷里的 `towerMode` 见「未宣称完成」）。

| v2 依据 | 引擎改动 | 判定 |
| --- | --- | --- |
| `towerOps.ts:9-19`（`TowerModeEnter` durable，`{agentId, sessionId?}`）+ `towerService.ts:420-426`（`tower_mode_enter` telemetry `{outcome:'entered'}`） | `execute_tower_init` 增 `callbacks` 入参（dispatch 传 `self.callbacks`），成功后发 `tower_mode.enter {agentId, sessionId}` 与 `tower_mode_enter {outcome:"entered"}` | 等价 |
| `towerOps.ts:21-30`（`TowerModeExit` durable，`{agentId}`）+ `towerService.ts:519-526`（`tower_mode_exit` telemetry `{reason}`） | `execute_tower_teardown` 同样增参，**非 dry-run** 成功后发 `tower_mode.exit {agentId}` 与 `tower_mode_exit {reason:"user"}` | 等价 |

**验证**：

- 新增 `tower_init_and_teardown_emit_mode_events_and_telemetry`：`EventCapture` 实现 `HostCallbacks`（覆写 `emit_event` / `telemetry`），真实 git 仓库上跑 init → 断言 enter 事件含 agentId/sessionId 与 enter telemetry，再跑 teardown → 断言 exit 事件与 `reason:"user"` telemetry。`cargo test --lib tools::tower::tests` → **16 passed / 0 failed**；`cargo test --lib tower::` → **136 passed / 0 failed**；`cargo test --lib tools::` → **1265 passed / 0 failed**。
- `rustfmt --check` 0；`cargo clippy --lib` 无告警。
- 门禁：`check:parity` 0、`check:engine-i18n` 256/241、`scan:hardcoded:rust` 3856/3856。

**未宣称完成**：

1. **跨会话 takeover**：v2 会调 owner 会话的 `tower.exit('takeover')`（在 owner 的事件总线上发事件）；引擎没有跨会话事件通道，只能由 REST takeover 路由写 owner 的 tower 域——这层需要设计。
2. **状态载荷里的 `towerMode`**：v2 由 `towerKey` 事件折叠得出；引擎的 `server/engine.rs::status_payload` 目前没有 `towerMode`（只有 `planMode`/`swarmMode`），而 `route_sessions.rs:153` 的列表字段取的是 flag 而非 tower 工作区状态。语义（「模式开着」还是「工作区存在」）需要先定。
3. `tower_mode_exit` 的 `reason` 在引擎里恒为 `"user"`——`force` / `exclude` 走的是同一个出口，v2 也没有更细的取值，但 mode-mutex 触发的退出（若将来接）需要别的 reason。

### 批次：operationLog 模型与磁盘存储（2026-11-13）

用户在选择题里选了 operationLog。本批落地前两层，`repositoryViewService`（868 行）与 `operationService`（335 行）随后。

| v2 依据 | 引擎落点 | 判定 |
| --- | --- | --- |
| `features/operationLog/operationLog.ts`（127 行：常量、OperationGitHead/OperationView 及结构等价、OperationKind/Origin/Record/Head/Retention、11 个错误码与 details） | `tools/operation_log.rs` 1:1 移植；`refs` 用 `BTreeMap`（v2 的 `operationRefsEqual` 本就按排序比较）；`OperationLogError` 带 `code`/`message`/`details`，实现 `Display` + `Error` | 等价 |
| `operationLogStore.ts:32-108`（sha256 内容寻址、`encoded = JSON + "\n"`、三个形状校验器） | `operation_log_store.rs` 的 `sha256` / `encode` / `is_operation_head` / `is_stored_operation_view` / `parse_operation` | 等价 |
| `operationLogStore.ts:111-260`（构造与 retention 校验、capture/undo/redo/restore/restoreBefore） | `OperationLogStore::new` / `capture` / `undo` / `redo` / `restore` / `restore_before`；head 迁移逐分支对齐（undo 的 `undoTargetId` 取 `target.parents[0]`，redo 出栈，restore 清空 redo 栈） | 等价 |
| `operationLogStore.ts:300-390`（`append`：视图 id、beforeView 与父视图一致性、nonce、id=sha256(content)、先校验保留再写） | `append` 同序；**操作文件存 `{id, ...content}`**（v2 的 `withId`），id 为 content 的 sha256 | 等价 |
| `operationLogStore.ts:390-470`（三级保留：条数/年龄/字节，字节含全部视图文件） | `assert_retention` 同三级，错误 details 带 `limit`/`maximum` | 等价 |
| `operationLogStore.ts:495-600`（不可变写：同内容复用、异内容 corrupt；原子写：temp + rename；进程内 Map 锁 + 文件锁，30s stale 回收、10s 心跳） | `write_immutable` / `write_atomic` / `with_lock`（全局 `LOCAL_LOCKS` 按 root 复用 Arc<Mutex>）/ `with_file_lock`（`create_new` 抢锁、stale 回收、心跳 spawn_blocking 触 mtime） | 等价 |

**验证**：

- 新增 10 条测试（4 模型 + 6 存储）：结构等价、保留默认值、错误码词表；capture 后 list 沿 parents、head 迁移、undo/redo/restore 的栈变化、不可变对象复用与 corrupt 检出、保留上限、空日志与首操作不可 undo、beforeView 与 head 不一致。`cargo test --lib -- tools::operation_log` → **10 passed / 0 failed**；`cargo test --lib tools::` → **1275 passed / 0 failed**。
- `rustfmt --check` 0；`cargo clippy --lib` 对新模块无告警。
- 门禁：`check:rust-module-deps` 415/415、`check:parity` 0、`check:engine-i18n` 256/241、`check:locale-orphans` 680/680 全绿；`scan:hardcoded:rust` 3888/3888（`--update` 前 +32 new，全部位于 `operation_log_store.rs`）。

**过程中修掉的两个真 bug**（都是移植时写错、由测试暴露）：

1. 操作对象只写了 content，漏了 v2 的 `id` 字段——`parse_operation` 回读时 `record.id != id` 直接报 corrupt。改为存 `encode(&record)`。
2. `OperationView` 漏了 `#[serde(rename_all = "camelCase")]`，`indexTree` 序列化成 `index_tree`——既与 v2 的词表不符，又让 `is_stored_operation_view` 误判 corrupt。

**历史中间态**：本节记录的是当时的分层落地情况，其中“等价”判断已被后续复核推翻；service/repository/snapshot 的实现、接线与最终证据以文件顶部「operationLog 全量复核与真实接线（2026-11-13，终态）」为准。

### 批次：operationLog 的 git 视图服务（2026-11-13）

补上让前两层活起来的第一块：`src/tools/operation_log_repository.rs`，按 v2 `repositoryViewService.ts` 的**核心**移植（不含 `readFiles`/`readBlobs`——那是给操作详情显示文件内容用的，留到接 `operationService` 时一起）。

| v2 依据 | 引擎落点 | 判定 |
| --- | --- | --- |
| `repositoryViewService.ts:156-192`（`open`：rev-parse 取 root/gitDir/objects，storageRoot 默认 `<gitDir>/kimi-operation-log`，`refsFromFiles` 探测） | `RepositoryViewService::open` 同参；shadow index 用每次 open 的随机目录，`dispose()` 删除 | 等价 |
| `:200-207,529-549`（`captureView`：先查 recovery 标记，再在 capture 锁内 `read-tree` → `add -A -- .` → `write-tree`，并行取 refs / 实际 index tree / 未完成任务） | `capture_view` / `capture_current_view`；shadow index 与 shadow object dir（`GIT_OBJECT_DIRECTORY` + `GIT_ALTERNATE_OBJECT_DIRECTORIES`） | 等价 |
| `:399-446`（`changedFiles` = `git diff --name-only -z`；`restoreView`：恢复前校验 → 写 recovery 标记 → `applyView` → 失败回滚 → 清标记） | `changed_files` / `restore_view`；回滚失败报 `recovery-required`，标记删除失败同样报 | 等价 |
| `:446-470`（`assertRestorable`：recovery 标记、未完成的 merge/rebase、实际 index 脏、目标视图含暂存改动） | `assert_restorable` 同四道，details 带 `operation` | 等价 |
| `:550-600`（`applyView`：`read-tree` + `read-tree --reset -u`、HEAD 的 detached/symbolic 分支迁移、refs 更新、index tree 回写） | `apply_view` / `update_refs`（`update-ref --stdin` 批量增删改） | 等价 |
| `:600-700`（HEAD 与 refs 的文件解析：symref 链、packed-refs、松散 refs、非法即回退 git） | `read_head_file` / `resolve_ref_file`（迭代化，避免 async 递归）/ `read_refs_from_files` / `read_packed_refs`；任一路径不可用则回退 git | 等价 |
| `:700-800`（`readActualIndexTree` / `actualIndexIsClean` / `treeForHead` / `emptyTree` / `readUnfinishedOperation`） | 同名方法；未合并条目时 index tree 返回空串（v2 同语义） | 等价 |
| `:472-528`（capture 锁：进程内 Map + 文件锁，30s stale、10s 心跳、5s 超时） | `with_capture_lock` / `with_capture_file_lock` | 等价 |

**验证**：

- 新增 5 条端到端测试（真实 git 仓库）：capture 记录 head/refs/tree/indexTree；`changed_files` 列出两视图差异；`restore_view` 回放目标树并清除 recovery 标记；暂存改动与未清除的 recovery 标记各自被拒；`unfinishedOperation` 的视图被拒且 details 正确。`cargo test --lib -- tools::operation_log` → **15 passed / 0 failed**（4 模型 + 6 存储 + 5 视图）。
- `cargo test --lib tools::` 第一次出现 1 条失败，**复跑 1280 passed / 0 failed**——按仓库记录判为负载抖动。
- `rustfmt --check` 0；`cargo clippy --lib` 对新模块无告警。
- 门禁：`check:rust-module-deps` 415/415、`check:parity` 0、`check:engine-i18n` 256/241 全绿；`scan:hardcoded:rust` 3910/3910（`--update` 前 +22 new，全部位于 `operation_log_repository.rs`）。

**历史中间态**：本节记录时的 service、readFiles/readBlobs 与生命周期缺口已在后续批次补齐；最终行为、真实接线和验证结果以文件顶部「operationLog 全量复核与真实接线（2026-11-13，终态）」为准。

### session/workspace 逐文件对齐续批（2026-11-14）

本批只修改当前逐项核对的独立 Rust 文件；不修改旧厚容器，不做服务接线或公开接口调整，不运行编译、测试、格式化及门禁。

| v2 文件 | 独立 Rust 文件 | 文件内对齐 | 仅登记的接线/公开接口差异 |
| --- | --- | --- | --- |
| `session/sessionActivity/sessionActivity.ts` | `session/sessionActivity/sessionActivity.rs` | 枚举、状态、事件字段及 serde 词表一致，无源码修改 | v2 `ISessionActivityView` 服务标识未对应；`lastActiveAt(): number` 与 Rust `i64` 的公开数值域差异保留 |
| `workspace/workspaceInstance/workspaceInstanceManagerService.ts` | `workspace/workspaceInstance/workspaceInstanceManagerService.rs` | `acquire` 的 workspace 后续 materialize/retain 改为独立任务，不再依赖调用方 poll；`onDidChange` 使用 `IndexMap` 保持注册顺序，退订使用 `shift_remove`，逐监听器经 `safely_call_listener` 隔离 panic；dispose 完成后清空监听并使后续订阅无操作 | v2 App/OnScopeCreated 注册未接；共享 future 的 `SharedManagerError` 仍把原始错误擦成字符串，保留任意原始错误对象需调整共享错误公开边界 |
| `workspace/workspaceInstance/workspaceInstanceManager.ts` | `workspace/workspaceInstance/workspaceInstanceManager.rs` | ref/change/snapshot 字段及 manager 方法集合一致，无源码修改 | v2 服务标识未对应；`WorkspaceInstanceLease.dispose(): void` 与 Rust 消耗 `self` 的 `FnOnce` 公开调用边界不同，保留 |
| `workspace/workspaceInstance/workspaceInstance.ts` | `workspace/workspaceInstance/workspaceInstance.rs` | live context 动态 metadata、activate 状态迁移、snapshot 及 dispose 的 closing/disposed 和错误传播一致，无源码修改 | 无 |
| `workspace/workspaceContext/workspaceContext.ts` | `workspace/workspaceContext/workspaceContext.rs` | context 字段集合及默认 `remoteCwd=None` 一致，无源码修改 | v2 服务标识与平坦 seed 接线未对应；`kind` 的 `'managed' / 'implicit'` 在 Rust 为任意 `String`，两个时间字段为 `i64`，均属保留的公开类型差异 |

