# 解耦契约 + Rust UI 骨架设计

> 状态：设计稿，待评审。本文档不含实现代码，不改动任何源码。
> 目标读者：kimi-agent（Rust 引擎）与 apps/kimi-code（TS 宿主）的维护者。
> 已定方向：先解耦定契约，再从框架补骨架，最后按垂直切片填功能；TUI 是第一优先级（不是 Web）。

## 结论先行

1. **三个契约都已经有天然的落点，不需要发明新协议。** 引擎→UI 事件契约直接复用已存在的 `EngineEvent`（[types.rs:19](../../packages/kimi-agent/src/events/types.rs)），它已是 serde 打标签枚举、同时被 TSFN 与 WS 两条通道序列化；条目身份契约用一个 `EntryId` 结构替代 `instanceof` 寻址（现状见 [undo.ts:508](../../apps/kimi-code/src/tui/commands/undo.ts)）；渲染后端契约是把 [tui.ts](../../packages/pi-tui/src/tui.ts) 里 `TuiBase` 的公开面（`addChild / removeChild / setFocus / start / stop / requestRender`，L437–L451）提升为一个 Rust trait，配两个实现（直通 scrollback / 全屏 alt-screen），共享同一棵组件树。
2. **状态归属的核心判断：transcript（对话条目流）的唯一权威应该搬回引擎侧，UI 只保留"视口状态"。** 当前 TUI 侧镜像了大量引擎语义（undo 545 行、流式增量 913 行、会话事件处理 1527 行、回放 917 行），这是解耦的主要收益区，也是主要风险区。
3. **第一个垂直切片刻意做小：启动 → 单 turn → token 流 → Esc 中断。** 不含 markdown、鼠标、搜索、undo、审批、回放。

### 数字口径说明

本文引用的行数为 2026-11-03 工作区（含未提交改动）实测值，与此前一轮摸底数字有个别漂移，以本次实测为准：`TuiMainScreen` 698 行（[tui-main-screen.ts](../../packages/pi-tui/src/tui-main-screen.ts)）、`TuiAltScreen` 1754 行（[tui-alt-screen.ts](../../packages/pi-tui/src/tui-alt-screen.ts)）、`TuiBase` 1531 行（[tui.ts](../../packages/pi-tui/src/tui.ts)）、`session-event-handler` 1527 行、`streaming-ui` 913 行、`session-replay` 917 行、`staging-leases` 313 行、`message-dispatch` 776 行、`transcript-renderer` 553 行、`undo.ts` 545 行（均位于 `apps/kimi-code/src/tui/`）。其余未亲测的数字沿用摸底结论并标注。

---

## 1. 三个契约的定义

### 1.1 引擎→UI 事件契约

**结论：不新建事件体系，把 `EngineEvent` 正式立为引擎与一切 UI（TUI / Web / ACP）之间的唯一事件契约，UI 侧只加一个薄的投影层 `UiProjection`。**

`EngineEvent` 已经满足契约的硬要求：serde `tag = "type"` + snake_case 线格式（[types.rs:17-19](../../packages/kimi-agent/src/events/types.rs)），每个事件携带 `agent_id` 与 `turn_id`（设计意图注释见 L8-16：宿主凭 `agent_id` 隔离 subagent 事件），当前线名如 `assistant.delta` / `thinking.delta` / `tool.call.started` / `turn.started` / `turn.ended`。它与两条现有通道的关系是"一份枚举、两种序列化"：

- **napi TSFN 通道**：`emit_event_cb` / `turn_event_cb`（[turn.rs:92-98](../../packages/kimi-agent/src/napi_bindings/turn.rs)），进程内直推。
- **WS 通道**：由 [ws-event-contract.json](../../packages/kimi-agent/ws-event-contract.json) 约束线格式。

**事件分类与方向规则**（提议，可评审）：

| 类别 | 例子 | 方向 | 规则 |
| --- | --- | --- | --- |
| 通知（notify） | `assistant.delta`、`tool.progress`、`turn.ended`、`cron.fired` | 引擎→UI 单向 | UI 不得回包；丢失即致命（见下） |
| 快照（snapshot） | `turn.ended`（含 usage/timing）、`tool.native`（完整结果） | 引擎→UI 单向 | 幂等 upsert，重复到达必须无害 |
| 交互请求（request） | `check_permission`、`ask_question`、`checkpoint`、`state_read/write` | 引擎→UI 请求，UI→引擎回包 | 必须携带关联 id（napi 现状为 `callbackId: number`，见 [turn.rs:90-101](../../packages/kimi-agent/src/napi_bindings/turn.rs)），回包必须恰好一次 |
| 控制（control） | UI→引擎：`prompt`、`steer`、`cancel(Esc)`、`undo` | UI→引擎单向命令 | 引擎处理结果通过通知类事件回流，UI 不等待同步返回值表达成功 |

**幂等与顺序保证**（提议）：

- 每个事件在 `(agent_id, turn_id)` 作用域内携带单调递增 `seq: u64`。**未核实**：`EngineEvent` 当前各变体没有统一 `seq` 字段，只有 `step: u32`（`llm.step.*`）与 `tool_call_id`。验证方法：`git grep -n "seq" packages/kimi-agent/src/events/`。若确认缺失，这是契约落地前必须补的字段。
- delta 类事件（`assistant.delta` / `thinking.delta` / `tool.call.delta`）按 `(turn_id, step, part_index)` 追加，天然满足"同键追加可交换"；快照类事件按主键 upsert，满足幂等。
- 顺序保证分级：同一 `(agent_id, turn_id)` 内严格有序；跨 turn、跨 agent 不保证顺序，UI 必须按键寻址而不是按到达顺序追加。
- 丢事件策略：当前 TSFN 用 `ErrorStrategy::Fatal`（[turn.rs:107](../../packages/kimi-agent/src/napi_bindings/turn.rs)），即宁可崩溃不可静默丢事件。契约沿用此立场：**事件流要么完整要么终止会话**，不支持断流续传。

**错误处理**：交互请求的回包从今天的 `Result<T, String>`（如 [callbacks.rs:121](../../packages/kimi-agent/src/callbacks.rs)）升级为结构化错误：

```rust
pub enum UiReplyError {
    Rejected,            // 用户拒绝（权限审批的 deny）
    Cancelled,           // 用户在交互中途按了 Esc
    Timeout,             // UI 侧超时（避免引擎永久挂起）
    Unsupported(String), // 能力缺失，fail-open 语义对齐 CHECKPOINT_UNSUPPORTED（callbacks.rs:24）
}
```

**UI 侧投影层**：UI 不直接消费 `EngineEvent`，而是消费投影后的 `EntryOp`（见 1.2）：

```rust
pub enum EntryOp {
    Upsert { id: EntryId, content: EntryContent },   // 新建或整体替换
    Append { id: EntryId, delta: String },           // 流式追加
    Seal   { id: EntryId },                          // 条目定稿，禁止再改
    Remove { id: EntryId },                          // undo 裁剪
    Barrier,                                         // 顺序栅栏（调试用）
}
```

投影层（`EngineEvent → Vec<EntryOp>`）是纯函数，可脱离终端单测。它是契约的一部分：事件语义变更只需要改投影层和它的测试。

### 1.2 条目身份契约

**结论：用结构化 `EntryId` 替代 `instanceof` 寻址；id 由"产生该条目的一侧"分配，引擎条目与 UI 本地条目分命名空间。**

现状问题：[undo.ts:508-516](../../apps/kimi-code/src/tui/commands/undo.ts) 用 `child instanceof UserMessageComponent || child instanceof SkillActivationComponent || child instanceof PluginCommandComponent` 判断"undo 锚点"。这把语义（哪些条目属于一个 turn）绑死在组件类继承链上：换渲染框架即全盘失效，且无法表达"跨组件的逻辑条目"（一个 assistant 消息由正文 + 多个工具调用卡片组成）。

**提议的形状**：

```rust
pub struct EntryId {
    pub ns: EntryNs,        // Engine | UiLocal
    pub turn_id: Option<String>,  // UiLocal 条目为 None
    pub seq: u64,           // 命名空间内单调递增
    pub kind: EntryKind,    // UserMsg | AssistantMsg | ToolCall | Thinking | Status | Banner | ...
}

pub enum EntryNs { Engine, UiLocal }
```

- **id 生成**：引擎条目在引擎侧分配（引擎已有 `turn_id` / `tool_call_id`，补一个 turn 内 `seq` 即可）；UI 本地条目（状态行、banner、错误提示）由 UI 从 `UiLocal` 命名空间分配，引擎永远看不到。
- **生命周期**：`Upsert`（创建）→ `Append`*（流式生长）→ `Seal`（定稿）→ 可能被 `Remove`（undo）。`Seal` 之后到达的 `Append` 是契约违例，UI 应记录并丢弃（不是 panic——跨进程边界要容错）。
- **与流式替换的关系**：流式期间的"思考中→正文""工具参数增量→完整结果"都作用于同一 `EntryId`，投影层负责把 `tool.call.delta`（增量）与随后的 `tool.native`（完整结果）折叠成对同一条目的 `Append` + 最终 `Upsert`。
- **与 compaction 的关系**：compaction 边界本身是一个引擎条目（`kind: CompactionBoundary`）。回放时引擎按新 transcript 重新发 `Upsert` 序列，UI 无需理解压缩逻辑。

**为什么这能支撑 undo 双轨裁剪**：当前 undo 是两条独立的轨道——引擎侧靠 `checkpoint_state` 快照恢复持久状态（v2 undo anchor，见 [callbacks.rs:126-131](../../packages/kimi-agent/src/callbacks.rs)，本地 StateStore 快照为权威，[callbacks.rs:1759-1764](../../packages/kimi-agent/src/callbacks.rs) 注释），UI 侧靠 `instanceof` 遍历组件树做裁剪（undo.ts:496-505）。两轨靠"同一时刻被触发"隐式同步，没有任何共享标识。有了 `EntryId` 之后，引擎在 undo 完成后发一个 `undo.applied { upto: EntryId }` 事件，UI 按 `(ns=Engine, turn_id, seq > upto.seq)` 精确删除——语义从"类继承"变成"区间裁剪"，双轨合一为"引擎主导、UI 跟随"。引擎侧 undo 的持久层已有 JSON Patch 级的基础设施（[patch.rs:79](../../packages/kimi-agent/src/session/patch.rs) `JsonPatchSet`，含 move 逆操作测试），`undo.applied` 事件可以直接挂在那个提交点上。

### 1.3 渲染后端契约

**结论：把 `TuiBase` 的公开面提升为 Rust trait `RenderBackend`；`TuiMainScreen`（直通 scrollback）与 `TuiAltScreen`（全屏）是它的两个实现；模式开关落在应用启动参数，组件树对后端透明。**

`TuiBase` 当前的公开面（[tui.ts:437-451](../../packages/pi-tui/src/tui.ts)）：`addChild` / `removeChild` / `setFocus` / `start` / `stop` / `requestRender`，加抽象 `doRender`（L517）与生命周期钩子（L519-527），以及鼠标分发（L822-833）与 overlay 合成（L1354）。提议的等价 trait：

```rust
pub trait RenderBackend {
    // 组件树挂载（等价 addChild/removeChild，但操作的是 EntryId 寻址的节点）
    fn mount(&mut self, node: ComponentNode, parent: Option<NodeId>) -> NodeId;
    fn unmount(&mut self, id: NodeId);

    // 渲染循环
    fn request_render(&mut self, force: bool);          // 等价 requestRender(force)
    fn start(&mut self) -> io::Result<()>;
    fn stop(&mut self, opts: StopOptions) -> io::Result<()>;

    // 输入与焦点
    fn dispatch_input(&mut self, ev: InputEvent);       // 键盘/粘贴/鼠标统一入口
    fn set_focus(&mut self, id: Option<NodeId>);

    // 能力上报：组件据此降级（见下）
    fn capabilities(&self) -> CapabilitySet;
}

pub struct CapabilitySet {
    pub scrollback_passthrough: bool,  // 直通模式：依赖终端原生滚动
    pub mouse_capture: bool,           // 全屏独占：自管视口滚动
    pub text_selection: bool,          // 全屏自绘选择
    pub search: bool,                  // alt-screen-search（现状 306 行）
    pub osc8_links: bool,
    pub inline_images: bool,           // kitty/sixel/iterm2
}
```

**两个后端如何共用同一组件树**：组件树是纯数据 + 纯渲染函数（输入尺寸与主题，输出行序列），不持有任何终端句柄；后端持有树的所有权并驱动渲染。直通后端只把"新增行"追加到终端 scrollback（等价 `TuiMainScreen` 的增量 diff 渲染），能力集里 `mouse_capture=false`、`search=false`；全屏后端接管整个屏幕（等价 `TuiAltScreen`），自己实现视口、滚动、搜索、选择。组件在两边的代码路径完全一致，差异全部收敛在 `CapabilitySet` 的分支里——这正是现状（app 侧两模式共享 244 个文件、模式分支 <100 行）验证过的可行结构。

**模式开关落在哪**：落在应用启动（构造）参数，与现状一致（`'regular' | 'fullscreen'` 是构造参数，切换需重启）。骨架期不做运行中热切换；但契约不阻碍它——`stop()` + 换后端 + `start()` 即可，组件树与 transcript 状态都不在后端里，热切换是后续可选项而非架构债。

---

## 2. 状态归属表

分类口径：**留 UI** = 纯视口/交互状态；**搬回引擎** = 会话语义状态，UI 只是它的投影；**两端各一份** = 引擎为权威、UI 缓存只读副本用于渲染。

| 状态 | 现状位置（行数为 2026-11-03 实测） | 归属 | 依据 / 说明 |
| --- | --- | --- | --- |
| transcript 条目模型 | `transcript-renderer.ts` 553 行 + 各 messages 组件 | **搬回引擎** | undo/compaction/replay 的权威都在引擎（patch.rs、checkpoint_state）；UI 只持有按 `EntryId` 索引的渲染缓存 |
| 流式增量缓冲 | `streaming-ui.ts` 913 行 | **搬回引擎**（增量聚合进引擎），UI 侧只留"当前帧待刷新的脏条目集合" | delta 聚合是幂等语义问题，放 UI 导致每个前端各写一遍 |
| undo 锚点/裁剪 | `undo.ts` 545 行（L508 instanceof） | **搬回引擎** | 引擎发 `undo.applied { upto }`，UI 区间裁剪；见 1.2 |
| 会话事件路由 | `session-event-handler.ts` 1527 行 | **拆分**：事件→EntryOp 投影是契约一部分（随引擎走），剩余纯 UI 副作用（响铃、标题栏）留 UI |
| 会话回放 | `session-replay.ts` 917 行 | **搬回引擎** | 回放 = 引擎把持久 transcript 重发为 `Upsert` 序列，UI 不需要专门回放逻辑 |
| 消息队列 / steer | `message-dispatch.ts` 776 行 | **搬回引擎** | 队列是 turn 调度语义；引擎已有 goal queue 先例 |
| staging leases | `staging-leases.ts` 313 行 | **搬回引擎** | staging 是"未提交输入"的会话状态，崩溃恢复需要它在引擎侧 |
| 权限审批交互 | `reverse-rpc/approval/*`（摸底 433 行） | **两端各一份**：请求/决策走引擎↔UI 交互契约（1.1）；面板布局、按键导航留 UI |
| 提问回调 | `reverse-rpc/question/*`（摸底 ~140 行） | 同上 |
| 编辑器缓冲区 | `components/editor/custom-editor.ts`（摸底 2254 行） | **留 UI** | 纯本地交互状态（光标、选区、补全） |
| 主题/配色 | `theme/*` | **留 UI** | 引擎不应感知颜色 |
| 视口滚动/搜索/选择 | `TuiAltScreen` 内（摸底 ScrollView 202 / search 306） | **留 UI** | 纯视口状态 |
| 活动面板 / 子代理活动 | `activity-pane.ts` 250 行、`subagent-activity-store.ts` 381 行 | **两端各一份**：活动事实来自引擎事件，面板开合状态留 UI |
| usage / telemetry 展示 | `usage-panel.ts` 等 | **两端各一份**：数值权威在引擎（`turn.ended` 快照），格式化留 UI |
| cache-hint / notify / survey | `cache-hint-controller.ts` 572 行、`notify.ts` 296 行、`survey-controller.ts` 815 行 | **留 UI**（首期不搬），后续逐项评估 | 与 turn 语义弱相关 |

---

## 3. crate 边界与依赖方向

**结论：新增 Rust UI 是一个独立的 cargo workspace，位于 `packages/kimi-tui-rs/`；契约类型放在最底层 `kimi-ui-core`，它只依赖共享 schema  crate，不依赖引擎实现，从而保持 `app > sdk > engine > shared` 分层无环。**

当前 `packages/kimi-agent` 是单 crate（Cargo.toml 为 `[package]` 非 workspace），引擎内部按 `src/` 下模块划分（events、turn_loop、session、callbacks 等）。若把 UI 直接做成 kimi-agent 的子模块会造成"app 层概念（终端渲染）反向沉进 engine 层"的分层违例。

**目录草图**：

```
packages/kimi-tui-rs/
├── Cargo.toml                  # workspace
├── crates/
│   ├── kimi-ui-core/           # 契约层：EngineEvent 镜像、EntryId、EntryOp、RenderBackend trait
│   │   └── src/lib.rs          # 公开面：仅类型与 trait，零终端依赖
│   ├── kimi-ui-components/     # ratatui 组件库：消息卡片、编辑器、对话框
│   │   └── src/lib.rs          # 依赖 kimi-ui-core + ratatui
│   ├── kimi-ui-backend/        # RenderBackend 实现：scrollback / alt-screen / headless(测试)
│   │   └── src/lib.rs          # 依赖 kimi-ui-core + crossterm
│   └── kimi-tui-app/           # 可执行：接线 napi 事件流 → 投影 → 组件树 → 后端
│       └── src/main.rs
```

**依赖方向**（单向，无环）：

```
kimi-tui-app → kimi-ui-components → kimi-ui-core → (shared schema)
kimi-tui-app → kimi-ui-backend    → kimi-ui-core
kimi-tui-app → kimi-agent (napi，仅运行时链接，无 cargo 依赖)
```

关键约束：`kimi-ui-core` 对 `EngineEvent` 采用**镜像定义**（同名 serde 枚举）而非直接依赖 kimi-agent，理由：(a) 避免 app 层 crate 反向依赖 engine 层 crate；(b) 让 UI workspace 可独立编译、独立出 crate 产物。镜像一致性由已有的 parity 检查门（Rust↔TS 接口 parity check）扩展覆盖——**未核实**：parity 脚本当前是否支持 Rust↔Rust 镜像对，验证方法：看 `scripts/` 下 parity 检查器的输入配置。备选方案是把 `EngineEvent` 抽到 `packages/shared-rs`（尚不存在，需新建）供双方依赖，二选一在实施阶段拍板。

与现有 TS 分层的关系：迁移期内 `apps/kimi-code`（TS TUI）与 `kimi-tui-app`（Rust TUI）并存，两者都位于 app 层、都只经 sdk/napi 触达引擎，互不依赖。

---

## 4. 第一个垂直切片

**范围：启动 → 建立引擎会话 → 发送一条 prompt → 渲染 token 流 → Esc 中断。**

包含：

1. `kimi-tui-app` 启动，加载 `kimi-agent` napi 库，创建会话（走通现有 `EngineSession` 创建路径）。
2. 直通 scrollback 后端（`CapabilitySet` 最小集）渲染：用户输入回显、assistant 正文流式追加。
3. `assistant.delta` → `EntryOp::Append` 投影落地。
4. Esc → UI→引擎 `cancel` 命令 → 收到 `turn.ended` → 流停止、输入框恢复可用。

验收标准（可观测、可测试）：

- **可观测**：token 增量逐帧可见（非整段一次性出现）；Esc 按下到渲染停止之间不超过一个渲染帧 + 一个引擎 tick；`turn.ended` 后终端 scrollback 中保留完整已输出内容。
- **可测试**：headless 后端（`RenderBackend` 的第三个实现，输出到内存缓冲）+ 录制的事件流回放，断言 EntryOp 序列与最终渲染文本；不依赖真实终端。

明确**不包含**：markdown 渲染、语法高亮、LaTeX、鼠标、搜索、文本选择、OSC8、图像、undo、权限审批、提问对话框、会话回放、slash 命令、subagent 事件隔离、全屏模式。

---

## 5. 迁移期间的上游 delta 政策

三个选项：

**A. TS TUI 全量跟随上游，Rust UI 另行追平。** 上游每个 TUI delta 先落 TS 侧（走现有 upstream delta 台账流程），Rust UI 从 TS 行为对齐。
代价：双份实现期内的 delta 工作量约 2 倍；但台账机制成熟、行为基准（TS）始终新鲜，Rust 侧永远有参照实现。

**B. TS TUI 冻结（只修 breakfix），新功能只进 Rust UI。** 上游 TUI delta 进台账但标记"冻结不移植"，仅在 Rust UI 达成 parity 后统一评估。
代价：迁移期内 fork 与上游 TUI 行为漂移持续扩大，parity 差距本身变成债务；若 Rust UI 进度不及预期，回退成本高。但迁移期总工作量最小，且符合"避免不必要特殊修改、与上游对齐"的既定原则——漂移被显式记账而非隐式发生。

**C. 双轨移植：每个上游 TUI delta 同时落 TS 与 Rust。**
代价：工作量最大且强迫 Rust UI 按上游节奏排期，丧失切片自主权；基本不可取。

**建议：B + 一条硬约束**——冻结只对 TUI 目录生效（`src/tui/**`、`packages/pi-tui/**`），引擎侧与 sdk 侧 delta 照常跟随，避免冻结范围蔓延。

---

## 6. 风险与未决点

1. **undo 双轨裁剪的原子性**（高风险）：引擎恢复持久状态与 UI 裁剪组件树之间没有事务。`undo.applied` 事件方案（1.2）把裁剪变成事件驱动的单向跟随，但需回答：undo 执行期间到达的在途 delta（引擎尚未 seal 的条目）如何处理？候选规则：`undo.applied` 携带事件流水位 `seq`，UI 丢弃水位之前的所有未消费事件。**未决，需在切片 2（undo 切片）前拍板。**
2. **流式增量替换的边界情形**（中风险）：thinking→正文的过渡、工具参数从增量到完整结果的折叠、`tool.native` 先于最后一个 `tool.call.delta` 到达的乱序。投影层必须用录制事件流做 property test，不能靠手测。
3. **编辑器重写代价**（摸底 2254 行）：含文件提及补全、粘贴突发处理、Kitty 键盘协议。ratatui 生态有 tui-textarea 可用作底座，但补全与粘贴突发需自研。预估为切片填肉期最大单项。
4. **markdown + LaTeX 重写代价**（摸底 2347 行）：markdown 可用 pulldown-cmark 打底，终端 LaTeX 渲染没有成熟方案，大概率降级为源码原样 + 高亮。**未决：LaTeX 降级是否可接受，需要产品口径拍板。**
5. **全屏能力在 ratatui 下的工作量**（中高风险）：ratatui 不提供 scrollback 文本选择、搜索高亮、OSC8 的现成实现；crossterm 提供鼠标捕获与 Kitty 键盘增强，但选择/搜索需自绘（现状对应代码 ~500 行，Rust 重写预计相当）。图像协议（kitty/sixel/iterm2）无 ratatui 内置支持，工作量大且终端碎片化，建议排到切片段最后。
6. **EngineEvent 缺统一 `seq`**：见 1.1，若确认缺失，补字段属于引擎协议变更，需按上游 delta 政策评估是否影响 WS 契约（[ws-event-contract.json](../../packages/kimi-agent/ws-event-contract.json)）。
7. **EngineEvent 镜像 vs 共享 crate**：见第 3 节，二选一未决。
8. **文档落点**：按 `docs/AGENTS.md` 的"Non-site documents"规则，`docs/` 根下内容会被 VitePress 构建发布，内部设计文档惯例应放 `reports/`。本文档按任务要求放在 `docs/design/`，若评审通过建议移到 `reports/design/` 或在 docs 构建中显式排除。**未决，需维护者拍板。**

---

## 7. 实施顺序

| 阶段 | 内容 | 退出条件 |
| --- | --- | --- |
| 0. 契约冻结 | 本文档评审通过；补 `seq` 字段（若缺失）；`EngineEvent` 镜像/共享方案拍板 | 契约文档合入；parity 门能挡住契约漂移 |
| 1. 骨架 | `kimi-tui-rs` workspace 四 crate 建空壳；`RenderBackend` trait + headless 后端；投影层纯函数 + 单测 | headless 后端能对录制事件流渲染出预期文本；`cargo check/clippy/fmt` 全绿；不触碰 TS 侧任何文件 |
| 2. 切片 1 | 第 4 节范围 | 满足第 4 节全部验收标准 |
| 3. 填肉（按风险排序） | undo 切片 → 审批/提问交互 → markdown → 编辑器 → 回放 → 全屏后端（滚动/搜索/选择）→ 图像 | 每个切片有独立验收；TS TUI 冻结解除以 Rust 侧 parity 为准 |
| 4. 切换 | Rust TUI 成为默认入口，TS TUI 保留一个版本周期作回退 | 双实现行为 diff 报告为空；上游 delta 台账中 TUI 条目全部闭环 |

每阶段沿用既定批次规则：独立批次、静态检查优先（cargo check/clippy/fmt + parity 门）、全量回归仅作提交前验收、不提交 commit 直至阶段收尾。

---

## 8. 增补：合批契约（2026-11-03 骨架阶段拍板）

> 实测依据：一个典型 turn（1,900 输出 token）触发 3,860 次 Rust→JS 跨界、232KB 序列化流量；单次通道 ~1.2µs、单次 TS 映射 ~1.0µs。瓶颈在事件基数与字节放大，不在单层实现。据此对 §1.1/§1.2 增补，本节与上文冲突处以本节为准。

1. **合批增量**：`EntryOp::Append` 携带合并段 `AppendDelta { text, span: SeqSpan, is_final }`，取代 §1.1 中单 token 的 `delta: String`。合并单位 = 冲刷触发（时间窗 **或** 字节上限 **或** turn/step 生命周期边界，先到先冲）；合并发生在**引擎侧**（单点合并，所有 UI 共享策略，投影层不各写一遍）。
2. **顺序保证**：段间按 `SeqSpan`（闭区间 `[first, last]`）接续校验（`first == prev.last + 1`）；gap/overlap 记为契约违例（`ContractViolation::SeqGap/SeqOverlap`），gap 段仍应用以保内容、overlap 段丢弃；跨 turn 顺序仍不保证。
3. **尾部收尾（防丢字）**：边界冲刷保证 `is_final = true` 的末段先于 seal 类快照事件发出；UI 在 `Seal` 时校验末段 final 标记，缺失记 `SealWithoutFinal` 违例，不 panic。
4. **新事件变体**：`stream.segment { agent_id, turn_id, stream, text, span, is_final }`（`stream`: `assistant | thinking | tool_call`）已在 `EngineEvent` 定型；骨架期只定义不发射，现有 per-delta 变体继续流动，行为切片再切换发射路径。
5. **transport-only 事件**：`llm.step.end` 在 UI 契约中**不投影**（TS 侧解析 usage/timing 后即丢弃，不构成渲染条目）；机制为 `UiVisibility::TransportOnly` + `TRANSPORT_ONLY_EVENTS` 常量清单，随行为切片扩展。
6. **合批参数**：`CoalescingPolicy { flush_interval: 16ms, max_pending_bytes: 8KiB, flush_on_boundary: true }` 为默认值 + 可配置入口（构造器/serde），数值**待实测调优**，不在本阶段拍死。
7. **seq 载体形态**：以信封 `SequencedEngineEvent { seq: Option<u64>, event }` 承载（默认 `None`，`skip_serializing_if` 保证未打戳时线格式零变化）；未做逐变体字段，原因是 `acp/` 测试含 6 处变体字面量构造点、本阶段不可触碰；待该范围解冻后可机械坍缩为变体字段。
