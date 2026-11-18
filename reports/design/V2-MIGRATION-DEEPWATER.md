# v2 迁移剩余深水区：设计梳理

状态：2026-11-17，全树 `cargo check --no-default-features --features cli` 约 1017 条
`: error`（会话起点 1297）。已完成并逐个对照 v2 的切片见
`reports/runbooks/v2-parity-ledger.md`。本文只处理剩余工作中**不能用局部补丁解决**的
四类结构性问题，给出方案、代价和推荐顺序。行为权威始终是
`D:\kimi\kimi-code\packages\agent-core-v2`（D fork v2）与
`.tmp/v2-ref-upstream`；两者冲突时记录，不私自选边。

## 1. 错误表示：`WireAsyncError`(Box) vs `SharedError`(Arc)

现象：`type WireAsyncError = Box<dyn Error + Send + Sync>`（`wire/record.rs:15`），
`type SharedError = Arc<dyn Error + Send + Sync>`（`_base/errors/errors.rs:96`）。
v2 只有一种 `Error`。两套表示在 `classify_turn_result`（`session/subagent/run_agent_turn.rs`）、
abort reason、turn 取消边界反复互相卡住。

实测修正（2026-11-17）：`Arc<dyn Error + Send + Sync>` **本身实现 `Error`**。
用独立 rustc 片段验证：`fn a2b(a: Arc<dyn Error+Send+Sync>) -> Box<dyn Error+Send+Sync> {
Box::new(a) }` 直接编译通过，且 `Display`/downcast 到内层具体错误均保留。反向
`Arc::from(box)` 亦成立。因此**不需要** downcast/payload 重建那套复杂方案；
纯 `.to_string()` 降级更无必要。

方案（推荐，A，简化版）：
1. 在 `_base/errors/errors.rs` 增加两个权威转换：
   `shared_to_wire(e: SharedError) -> WireAsyncError { Box::new(e) }` 与
   `wire_to_shared(e: WireAsyncError) -> SharedError { Arc::from(e) }`。
   两者都是零成本的所有权包装转换，信息完全保留。
2. 所有 `SharedError` ↔ `WireAsyncError` 边界调用它们，禁止散落的 `.to_string()` 降级
   （包括本次迁移中 `fullCompactionService.rs:985` 与 `run_agent_turn.rs` 的临时 String 包装，
   应改回结构保留版本）。
3. 不改 `WireAsyncError` 别名本身，故不触碰几百处 `Box::new(...)` 调用。

代价：两个一行 helper + 少量测试；收益：一次解开 subagent/classifier/abort 的多条错误。

备选 B：把 `WireAsyncError` 别名统一为 `SharedError`。更彻底，但要求全树把 `Box::new`
改为 `Arc::new`，blast radius 太大，本阶段不推荐。

## 2. `LLMMessage` → `ContextMessage` 词汇迁移

现象：`report/v2-parity/type-vocabulary-plan.md` 已证明 9 组类型中 `ContextMessage`
是 v2 权威（`agent/contextMemory/types.ts`），Rust `contextMemory/types.rs` 已是 v2 形状；
但 `turn_loop/types.rs` 仍 re-export kimi-core 的 legacy `LLMMessage`（`role: String`,
`content: String`, `blocks`, `origin: Value`）。`tool_select_state.rs` 已把 8 个函数迁到
`ContextMessage`，调用点（`tools/toolset.rs`、`agent/tool_select/tool_select_service.rs`、
`turn_loop/run_turn.rs`）未迁，产生约 9 条错误 + 大量潜在语义偏差。

关键事实：`to_llm_message`/`from_llm_message`（`llm_adapter/contract/message.rs:85,109`）
是 v2 已有的 **contract ↔ LLM** 边界转换，不是可随意发明的 `From`。v2 的 tool-select
shaping 发生在 tool-select service 内、读 context memory 的 `ContextMessage[]`；Rust 的
`run_turn.rs:814` 在 turn loop 里对 `Vec<LLMMessage>` 直接调 `shape_history`，是端口
自己引入的路径偏差。

方案（推荐）：
1. 把 `run_turn` 的 shaping 调用移到 `AgentToolSelectService`（对齐 v2 的
   `shapeHistory` 所在层），源数据用 `IAgentContextMemoryService::get() -> Vec<ContextMessage>`。
2. `toolset.rs` 的 `seed_loaded_tools_from_history` 改收 `&[ContextMessage]`；调用点在
   `run_turn` 里传 context memory 快照，而不是本地 LLM messages。
3. 不新增 `impl From<LLMMessage> for ContextMessage`；LLM 请求边界保留 contract 转换。
4. legacy `LLMMessage` 的删除作为最后一步，等所有消费者迁完。

代价：改 turn-loop 与 tool-select 的接口/调用链（约 3 个文件），需要行为测试验证
shaping 仍作用于同一历史。收益：消除词汇双轨，避免继续用旧字段糊调用点。

## 3. `AgentModelDefinition` 泛型化（领域模型类型可达）

现象：v2 `AgentSpace.use<D,R>(definition: D, run: (model: AgentModelInstanceOf<D>) => R)`
是**领域模型类型泛型**；Rust `use_model()` 非泛型，闭包只拿到 `&AgentModel`（state-machine），
`TokenCountingAgentModel::get/measured/...` 等领域方法不可达。`sessionTokenCountingService`
和 `sessionUsageService` 仍调不存在的 `use_`/`use_async`（约 24 条错误）。

已在本次迁移中验证：补一个非泛型 `use_sync(&AgentModel)` 会引入 14 条
"no method named get/measured" 新错误，因为领域方法根本不在 `&AgentModel` 上。该尝试已撤销。

方案（推荐）：
1. 给 `AgentModelDefinition` 增加 phantom 领域类型参数 `<M>`，定义处
   (`define_agent_model`) 仍用 `into_base()` 构造，但保存 typed 构造器
   `Arc<dyn Fn(AgentModelContext) -> M>` 或等价 typed accessor。
2. `AgentSpace::use_model<M,R>(&Arc<AgentModelDefinition<M>>, run: FnOnce(&M)->R)`：
   内部持有 typed `M` 实例（或 `Arc<M>`）而非仅 base；base 仍可导出给 wire。
3. 调用点 `use_`/`use_async` 改为新泛型 API：`use_model(def, |m| m.get(...))`；
   同步/异步由 `M` 的方法签名自然决定，不需要两个 API。
4. `AgentModel::dispose` 的 async 仍在 base 层由 lifecycle 处理，不影响 typed 闭包。

代价：agent-model 子系统结构性重构（`agent/state/agentModel*`、`agent/agent_context/agentSpace`、
各 `xxxAgentModel` 定义处、token counting/usage 消费者）。这是本清单中最大的一块，
必须在前两项（错误表示、词汇迁移）稳定后做，否则错误信号混杂。

补充证据（2026-11-18）：`TokenCountingAgentModel::into_base()` / `UsageAgentModel::into_base()`
消耗 wrapper 只返回 base，而领域状态 `Arc<Mutex<TokenCountingState>>` 只被注册到 base 的
applier 闭包持有——`base.committed_state` 不含 anchors/tokens。因此**无法从 base 重建**
领域 wrapper；`use_` 需要一个存活的领域实例句柄。最小侵入方向：在 `AgentModel` 上加
`domain: Option<Arc<dyn Any + Send + Sync>>`（Clone 时共享），构造时把领域状态 Arc 存入；
`AgentSpaceImpl::use_domain<T>(def, run: FnOnce(&AgentModel, &T) -> R)` 从 entry.model.domain
downcast 到具体 state 类型，领域方法改为接受 `(&AgentModel, &T)` 或重建轻量访问器。
这样不动 `AgentModelDefinition` 的泛型形状（blast radius 最小），但仍需同步/异步两种边界
和 lifecycle（dispose）一致性验证。不要在没有完整实现 + 验证前引入半成品。

进展（2026-11-18）：孤儿模块 `agent/state/agentModel.rs`（v2 无对应）已从活动图中
移除——`agentSpace.rs` 现在与权威 `state/agentModel.rs` 对齐（AgentSpaceHost/AgentModelBridge/
AgentModelContext/AgentModelDefinition 全部统一）。这是 typed accessor 的前置条件，已完成。
剩余：`use_`/`use_async` 的 9 个调用点仍需 (a) `agent_space_of()` 的 Result 解包，
(b) 领域对象存活句柄（domain Arc<dyn Any> + typed downcast）。

## 4. 全局 future-not-Send（约 50 条）

现象：rustc 指认 `parking_lot::MutexGuard`（`!Send`）跨越 `.await`；典型在
`agent/task/task_service.rs`（10 条）、`callbacks.rs`、`gitMemoryService`、
`session/subagent` 等。多数代码已经 `drop(guard)` 或在块内结束借用，但 async block 的
生命周期推断仍把 guard 视为跨 await（有时是 `Box::pin` / `Shared` 放大了生命周期）。

方案（推荐）：
1. 逐条看 human-format 诊断的 "value used across an await" 指向，把 guard 的作用域
   用显式块 `{ let g = lock(); ... }` 收窄到 await 之前；不要加 `Send` wrapper。
2. 对已经是 `Shared<BoxFuture>` 的 future，确认其输出类型不含 `!Send`（常见是
   `Rc`/`RefCell` 或 dyn trait 缺 `+ Send`），在类型别名处补 `+ Send`。
3. 这类问题没有统一一行修复；按文件逐个处理，但不需要 v2 决策。

代价：机械但分散；可穿插在其它切片之间做，不作为独立阶段阻塞。

## 推荐顺序

1. **第 1 项（错误表示）** 先做：范围小、收益大，且 subagent/classifier 的多条错误依赖它。
2. **第 4 项（future-Send）** 穿插：纯 Rust 问题，随各模块顺手修，不阻塞 v2 对齐。
3. **第 2 项（ContextMessage 词汇）**：改 turn-loop/tool-select 接口，需行为测试。
4. **第 3 项（AgentModelDefinition 泛型）** 最后做：最大重构，必须在前三项稳定后启动。

## 验收标准（每个切片）

- 对照 v2 源文件，逐函数确认语义一致（不是让编译器闭嘴）。
- `cargo check --no-default-features --features cli` 出口码与目标模块错误数真实下降。
- 不降低任何 gate 基线；不新增 `#[allow]`、不新增桥接 `From`（v2 已有的边界转换除外）。
- 每批更新 `reports/runbooks/v2-parity-ledger.md`，写清 v2 依据、真实出口码、未验证边界。
- 整树首次通过时，跑 `cargo test --no-default-features --features cli` 与相关 `check:*` gates。
