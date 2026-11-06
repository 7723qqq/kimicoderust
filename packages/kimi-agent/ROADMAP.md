# kimi-agent ROADMAP（新基点 2026-11-06）

> 本文件只记 2026-11-06 之后的事。§1–§59 的历史批次记录已冻结在
> `packages/kimi-agent/ROADMAP-ARCHIVE-2026-11-06.md`，需要查旧结论时去那里，不在本文件复述。

## 1. 新基点（2026-11-06）

- HEAD：`758456839`（`test(kimi-agent): give the environment a panicking-safe scope, drop the debug probes`）
- 分支 `main`，已推送 `origin`（`git@github.com:7723qqq/kimicoderust.git`；本机 HTTPS 到 github.com:443 超时，推送走 SSH）
- 下列数字均为 2026-11-06 当场执行命令读出，不抄旧文档快照：

| 检查 | 命令 | 出口码 | 输出 |
|---|---|---:|---|
| 引擎全量单测 | `cargo test --lib`（`packages/kimi-agent`） | 0 | 3633 passed / 0 failed（22.2s） |
| 引擎 lint | `cargo clippy --all-targets --features cli -- -D warnings` | 0 | clean |
| 引擎格式 | `cargo fmt --check` | 0 | clean |
| 聚合门禁 | `bun run check:all` | 0 | 19 道全绿 |
| TS 类型 | `bun run typecheck` | 0 | 全包 clean |
| 共享层单测 | `cargo test`（`packages/kimi-ui-core`） | 0 | 76 passed |
| 终端后端单测 | `cargo test --offline`（`packages/kimi-ui-terminal`） | 0 | 22 passed |
| 应用层单测 | `cargo test`（`packages/kimi-tui-app`） | 0 | 21 passed |
| 结构模型 | `check:architecture` | 0 | 21 modules / 1 warning（既有软依赖豁免） |
| 模块依赖图 | `check:rust-module-deps` | 0 | 373 `.rs` / 42 顶层模块 / 270 条边 / 3082 引用点 |
| 上游 delta | `check:upstream-v2-delta` | 0 | 99 条全 triaged：ported 59 / not-applicable 40 / **tracked 0**（上游 ref `a2afdbc7d` 2026-10-30） |

结构现状（本基点的起点，不改）：`packages/kimi-ui-core`（契约 + 组件树 + 三种 headless 后端）、
`packages/kimi-ui-terminal`（ANSI 渲染后端，零终端依赖）、`packages/kimi-tui-app`（输入解码 +
应用循环 + `UiTransport`）；引擎侧投影层在 `packages/kimi-agent/src/events/project.rs`。

## 2. 归档索引（只读，不再更新）

| 文件 | 覆盖范围 | 记的是什么 | 为什么冻结 |
|---|---|---|---|
| `packages/kimi-agent/ROADMAP-ARCHIVE-2026-11-06.md` | 2026-08-30 → 2026-11-06，§1–§59，10,763 行 | 全部批次记录：上游对齐、协议面、拆分、Rust UI、测试基建 | 新基点重开记录，旧结论只作检索 |
| `RUST-PORT-GAP.md` | 快照 2026-11-02（HEAD `b2c89a8fe`） | 上游 v2 移植缺口台账（A 组整层 + B 表 + §4 不一致清单） | 判据已改为 allowlist 实时 verdict，台账 tracked 已归零 |
| `DEAD-CODE-INVENTORY.md` | 快照 2026-11-02 | 死代码四层盘点与波次计划 | 盘点已执行完波次 1，剩余裁决点转入本文件 §3.2 |
| `DEVELOPMENT-OPTIMIZATION.md` | 快照 2026-11-02 | 全仓工程体检（结构/性能/缺陷/流程 + V1–V6 待验证） | 五阶段排期已被实际批次取代 |
| `DECISION-POINTS-2026-11.md` | 2026-11-03 | S4/S5/A4/A5/B-41/B-25 六项决策点卷宗 | 其中五项已于 §53.1 裁决落地，剩余项转入 §3.2 |
| `FRONTEND-RUST-ASSESSMENT.md` | 2026-11-03 | 前端 Rust 化可行性（TUI/Web/VSCode/daemon 顺序与条件） | 路线结论转入 §3.2，细则按需回查 |
| `FS-LIST-PROTOCOL.md` | 2026-11-03 | fs 列表协议面 F0–F4 设计与边界 | F0–F4 已落地，仅"明确不做"清单仍有效（§3.2） |
| `SPLIT-PLAN-kimi-agent.md` | 2026-11-03 | 五个大文件的拆分测绘与批次 | 批 1/3/4/5/6 已完成，批 2 转入 §3.2 |
| `debug-env-test-race.md` | 2026-11-03 → 2026-11-06 | 环境变量竞态调试会话（已 CLOSED） | 结论已落地（共享锁 + RAII 作用域），无剩余动作 |
| `HANDOVER-kimirust.md` | 2026-11-03 | 交接文档（基线、纪律、踩坑清单） | 纪律条款已并入 §4；状态快照已过期 |

后四份中的 `HANDOVER-kimirust.md` 被 `.gitignore` 的 `HANDOVER*.md` 忽略，其余五份是未跟踪
工作底稿：它们都不进提交，只在本机保留，且自 2026-11-06 起不再维护。

## 3. 承接项与待裁决项

### 3.1 待裁决：`EngineEvent` 走 Rust 侧镜像还是抽共享 crate（卡住 TUI 宿主的事件循环）

**事实**

- 引擎事件枚举 `EngineEvent` 定义在 `packages/kimi-agent/src/events/types.rs`（serde
  `tag = "type"` + snake_case），同时被两条通道序列化：napi TSFN 与 WS 契约。
- 引擎侧投影层 `packages/kimi-agent/src/events/project.rs` 把 `EngineEvent` 折成
  `EntryOp`；`packages/kimi-agent/tests/ui_slice_integration.rs` 用录制 JSON 驱动
  "事件 → 投影 → headless 后端 → 排版行"整链（7 测）。
- 契约层 `kimi-ui-core` **不含** `EngineEvent`，只含投影后的 `EntryOp` / `EntryId` /
  `EntryKind` 词表；`kimi-tui-app` 依赖的正是这一层。
- 现有门禁里没有 Rust↔Rust 的"镜像一致性"检查：`check:event-coverage` 管的是"23 类事件
  是否都归类"（14 projected / 9 transport-only），`check:parity` 管的是 Rust↔TS 接口面。
- 设计文档 §3 的分层要求是 `app > sdk > engine > shared` 单向，app 侧 crate 不得反向依赖
  engine crate。

**选项**

- (a) **Rust 侧镜像**：在 `kimi-ui-core` 里再定义一份同形 `EngineEvent` 子集，靠新写的
  Rust↔Rust parity 门禁挡漂移。代价：两套定义、两套测试，新增事件要改两处；这条门禁本身
  要新写（现无先例）。
- (b) **抽共享 crate**：把 `EngineEvent` 从 `kimi-agent` 移到更低层（例如
  `packages/kimi-ui-contract`），engine 与 UI 都依赖它。代价：一次机械搬迁，但
  `EngineEvent` 是 napi/WS 的线格式源头，搬迁会动 architecture 指纹与模块依赖边。
- (c) **app 只消费 `EntryOp`**：投影留在引擎侧，Rust TUI 通过 `UiTransport` 之外的事件
  通道收 `EntryOp` 流；app 根本不定义 `EngineEvent`。代价：Rust TUI 拿不到原始事件
  （step 进度、thinking 增量之外的元数据）时，仍要回到 (a) 或 (b)。

**影响面**

- (a) 只动 UI crate + 一个新门禁；(b) 动 engine 与 UI 两侧、线格式与指纹；(c) 不动现有代码，
  只决定"宿主把什么喂给 app"。

**建议**

选 (c)，并把 (b) 记为"真的需要 Rust 侧读原始事件时再做"。理由：`EntryOp` 已经是契约层
词表、投影已经是引擎侧权威，app 侧再定义一份事件枚举属于双份定义；共享 crate 是正确方向，
但在只有"渲染"需求时先付搬迁费没有收益。

### 3.2 未收口（不依赖上面的裁决）

- **应用层缺口**：raw mode 与 `crossterm` 事件循环、宿主侧 `UiTransport` 实现、编辑器语义
  （多行、光标、补全）。这三项是"启动 → 单 turn → token 流 → Esc 中断"切片的剩余部分。
- ~~**provider 环境变量未进共享锁**~~ **已关闭（2026-11-06，见 §5）**：`config/tests.rs` 的 10 处
  环境变量样板已改用 `EnvScope::unset`，持套件共享环境锁并 panic 安全恢复。
- **三条负载敏感用例**：`test_spawn_timeout_kills_tree`、`test_kill_managed_process`（进程退出
  事件）与 `test_unexpected_close_marks_failed_and_emits`（断连通知）在全量并发下偶发超时，
  单独跑均通过；本基点首轮全量为 0 failed，未按"放松预算"处理。
- **B-27 / fs watch 环境阻塞**：`notify` 依赖在本环境不可得，WatchService 不移植，保留显式
  `/reload`；相关上游 delta 已记 not-applicable。
- **acp 产品代码拆分（SPLIT-PLAN 批 2）**：`acp/mod.rs` 的产品代码仍在一处；拆开会让
  `scripts/scan-hardcoded-rust.mjs` 的测试区截断把一直跳过的产品 prose 首次暴露（26 条），
  需先决定台账口径。
- **S4 配置双源制度化**：TS 解析 → napi 透传的登记表 + 门禁未做；A8(b) Rust 单源要等 Rust
  原生入口裁撤，未裁。
- **fs 协议面明确不做**（继续生效）：anchored 绝对路径 list、条目形状规整（etag/child_count）、
  `pullRequest` 实查、`fs:search`/`fs:suggest` 的 gitignore/glob 落地。
- **死代码盘点的三个裁决点**：`wire-schema.ts` 19 个未接线 schema（建议补齐接线）、
  kosong/oauth 零消费导出（保留或收缩）、`apps/kimi-web` 源码去留。
- **前端路线**：TUI → daemon 边界收缩 → Web → VSCode 薄壳；各路线的"不该做"条件见归档的
  `FRONTEND-RUST-ASSESSMENT.md`，本基点只推进 TUI 切片。

## 4. 记录规约（本基点起）

1. 每条批次记录以「日期 + 出口码 + 落点」开头，正文写事实、判断依据与验证方式；不复述历史，
   需要引用旧批次时指向 `ROADMAP-ARCHIVE-2026-11-06.md`。
2. 验证分层：先最窄静态门禁与定向测试，再 `bun run check:all` + `cargo clippy` +
   `bun run typecheck`；全量回归只在交付前验收。红灯先做基线对照与窄化复现。
3. 台账与指纹只走官方通道：`scripts/check-architecture-drift.mjs --update`、
   `normify_module_refresh`、`check:rust-module-deps --update`、`scan:hardcoded:rust -- --update`；
   每次 `--update` 后复核新增/删除条目。
4. 反引号内以 test_ 开头的标识符会被 `check:roadmap-refs` 当作测试名校验，写之前先确认它
   在源码里真实存在；`file:line` 引用同理。
5. 提交：Conventional Commit、无 agent 署名、立即推送（SSH）。

## 5. 2026-11-06 配置测试的环境作用域（exit 0）

**落点**：`packages/kimi-agent/src/config/tests.rs`（10 处）、
`packages/kimi-agent/src/test_support.rs`（新增 `EnvScope::unset`）。

**做了什么**：`config/tests.rs` 里 10 处环境变量样板（`ANTHROPIC_BASE_URL` /
`OPENAI_BASE_URL` / `KIMI_BASE_URL` / `GOOGLE_GEMINI_BASE_URL` / loop 三个超时与步数 /
`KIMI_MODEL_THINKING_KEEP` / `KIMI_IMAGE_*`）原先是"快照 → 手动 unset → 手动 restore"，
现在统一成 `EnvScope::unset(&[..])`：持套件共享的环境锁，并在 `Drop` 里恢复。

**为什么**：两个理由，都与 §3.2 记下的同一处残留有关。

- 窗口是跨用例的：这些变量被别的模块在运行期读（例如子代理超时、图片预算），而
  "快照 → unset → 断言 → restore" 之间没有任何互斥，别的用例可能在这段窗口里读到被清空的
  值。
- 恢复不抗 panic：中途断言失败就把变量留在被清空的状态，污染整轮——与 §3.2 里
  `HOME`/`USERPROFILE` 那次同型。

**没有统一的部分**：`llm/thinking_guard.rs`、`server/remote_control.rs`、
`tools/github.rs` 仍用各自模块的锁；它们只写本模块独有、别处不读的变量，跨文件窗口不成立，
真要统一得先证明"谁在读"，留作独立批次。

**验证**（全部当场执行）：

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib config::` | 0 | 70 passed |
| `cargo test --lib`（全量） | 0 | 3633 passed / 0 failed（22.3s） |
| `cargo clippy --all-targets --features cli -- -D warnings` | 0 | clean |
| `cargo fmt --check` | 0 | clean |
| `bun run check:all` | 0 | 19 道全绿 |
| `check:rust-module-deps` | 0 | 270 → **271** 条边（新增 `config -> test_support`，按官方通道记录） |
| `check:architecture` | 0 | 21 modules（`kimi-agent` 指纹 `b3fbae663d56a9a4` → `9f3815fa8bb4f6e5`） |

## 6. 2026-11-06 补 Rust 之一：会话批准的记忆半边（exit 0）

**落点**：`packages/kimi-agent/src/permission/mod.rs`、`src/rpc/types.rs`、
`src/callbacks.rs`、`packages/node-sdk/src/native/sdk-client/build-handle.ts`。

**事实基线**（逐个对着 TS 实现核出来的）：

- 引擎只会**读**快照里的 `session_approvals`（`permission/mod.rs` 第 5 步
  `SessionApprovalHistory`，`compiled_session`），Rust 里没有任何写点；
- 唯一记录方是 TS：`packages/node-sdk/src/native/session-approvals.ts` 的
  `rememberSessionApproval`，写在 `NativeSessionMeta.sessionApprovals`，靠重建快照
  回灌；
- 决策回包 `PermissionDecision` 只有 `decision`/`reason`，宿主无法表达"记住这条规则"。

**改动**：

- `PermissionEngine` 新增自持清单 `session_records`（`RwLock<Vec<CompiledRule>>`）与
  `remember_session_approval(&self, rule) -> bool`、`session_approval_rules()`；
  第 5 步先查自持清单、再查快照，两者同权。
- `PermissionDecision` 新增 `remember: bool`（`serde(default)` + 不序列化 false），
  宿主在"允许且本会话记住"时置真。
- `callbacks.rs` 的 Ask 分支在返回允许后，用该次询问携带的
  `session_approval_rule` 写入引擎清单——现在宿主不保存任何清单，下一次同规则调用也
  不再询问。
- TS 侧 `checkPermission` 在 `scope === 'session'` 时回 `{decision:'allow', remember:true}`；
  原有本地清单保留（重建快照仍可携带）。

**顺带**：`PermissionDecision` 的字面量构造点（`callbacks/tests.rs` 14 处、
`llm/multi.rs` 4 处、`pipeline`/`server`/三个 guard/`turn_bench` 各 1 处）统一改用
`::allow()` / `::deny(reason)`，新增字段才不会散落到各 fixture。

**验证**（全部当场执行）：

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib permission::` | 0 | 65 passed（+3：记录后不再问、只覆盖自己的 pattern、重复/空规则不记） |
| `cargo test --lib callbacks::` | 0 | 49 passed（+1：宿主回 remember 后第二次调用不再询问） |
| 变异验证 | — | 把 `remember` 改回 `false` → 新用例在"引擎记录了规则"处失败；还原后通过 |
| `cargo test --lib`（全量） | 0 | 3637 passed / 0 failed（23.0s） |
| `vitest session-approvals + permission-acceptance` | 0 | 14 passed |
| `bun run typecheck` | 0 | clean |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run check:all` | 0 | 19 道全绿（`kimi-agent`、`node-sdk` 指纹刷新；normify `engine.core` 刷新） |

**下一步（同一条线）**：`parseBash`——引擎侧完全没有实现，TS 用
`bashTargetDirs` + `packages/tree-sitter-bash`（7 文件 / 5,000 行）解析 shell 命令给
AGENTS.md 操作数提醒用。这是 14 个宿主缝里唯一"Rust 侧为零"的一个。

## 7. 2026-11-06 补 Rust 之二：bash 操作数目录解析（exit 0）

**落点**：`packages/kimi-agent/src/tools/bash_target_dirs.rs`（新，784 行含测试）、
`packages/kimi-agent/tests/bash_target_dirs_corpus.rs`（新）、
`tests/fixtures/bash-target-dirs.json`（796 条金标语料）、`src/callbacks.rs`（新增
`supports_parse_bash`）、`src/napi_bindings/callbacks.rs`、`src/turn_loop/run_turn.rs`、
`packages/node-sdk/src/native/sdk-client/build-handle.ts`（注释纠错）。

### 7.1 TS 行为清单（对齐基准，含边界与失败路径）

入口 `bashTargetDirs(command, cwd, home) → string[] | null`：

| 环节 | 语义 |
|---|---|
| 解析 | 仓库自研解析器，预算 500ms / 10,000 节点；`ok=false` 或 `hasError` → `null` |
| 失败形态 | **不进回合错误**；`null` 与 `[]` 对消费者不可区分（`run_turn.rs` 只对 `Some(dirs)` 扩展） |
| 命令收集 | 仅沿 `program`/`list`/`pipeline`/`redirected_statement` 下钻；`pipeline` 标记传给子命令 |
| 命令名 | `command_name` 的第一个 named 子节点；含 `/` 或不可字面化 → 忽略整条命令 |
| 参数 | 名字之后**每个**子节点 `literalText`；任一不可字面化 → `dropped`（dot 兜底不再加） |
| 字面文本 | `word`（含 `$` `` ` `` `*` `?` `[` `]` `~` 判废，解 `\x` 转义后再判）、`number`、`raw_string`（去引号）、`string`（拼 `string_content`，出现其它 named 子节点即废）；其余类型废 |
| `cd` | pipeline 内跳过；dropped → 基准未知；恰一个非 `-` 参数 → 绝对 normalize / 相对 join；无参数 → `home`；其余 → 基准未知 |
| 列表命令 | `ls tree find dir exa eza lsd`；`find` 取前导路径（跳过 `-H/-L/-P/--`，遇 `-`/`(`/`)`/`!` 停），其余丢选项（两套取值选项；`--opt=value` 不吞） |
| 无操作数 | dropped → 不加；否则加 `.` |
| 路径 | `pathe`：反斜杠转 `/`、盘符大写、尾分隔符保留、相对路径保留前导 `..`、UNC 处理 |
| 去重 | 规范化后按首次出现顺序 |

### 7.2 实现选择与等价口径（已与用户确认：方案 B）

引擎侧用**真实 tree-sitter 语法**（`tree-sitter 0.27` + `tree-sitter-bash =0.25.0`，
钉住宿主解析器对照的那个版本），算法 1:1 复刻；`pathe` 的 `normalize`/`join`/`isAbsolute`
逐行移植（`std::path` 在这些输入上会给出不同字符串）。792 条语料中 779 条与宿主逐字相同，
**17 条登记偏差**全部是 `null` ↔ `[]` 互换（`case` 续行、空 `$()`、空反引号、`${!# }`、
heredoc 尾语句、`<>` 重定向、`$'` body 等），对提醒输出无差别；语料测试把 17 条按名登记，
新增偏差或偏差消失都会失败。

### 7.3 接线：宿主回调降为覆盖项

`HostCallbacks` 新增 `supports_parse_bash()`（默认 `false`）。`run_turn` 现在：
宿主声明支持 → 问宿主；否则用引擎自己的解析。`build-handle.ts` 的 `parseBash` 与
`napi_bindings` 分别声明支持，因此 napi 宿主仍走它的解析器，而 standalone server /
REPL / 任何没有该回调的宿主第一次真正拿到操作数目录（此前是"没有解析器就什么都不加"）。

### 7.4 其它宿主缝的对齐判定（本轮一并核过）

| 缝 | 判定 | 依据 |
|---|---|---|
| `parseBash` | **Rust 消费者级完全等价**（本轮补上） | 796 条语料 + 17 条登记偏差 |
| `llmChat` | **Rust 完全等价** | 配 provider 时引擎自己发 HTTP（`llm/`），宿主回调是旧代理；TS 实现是 fail-loud 桩 |
| `executeTool` | **Rust 完全等价** | `NATIVE_TOOL_NAMES`（98）+ 动态族 `mcp__*`（引擎 MCP manager）、GitHub 族都在 Rust 执行；只剩"宿主注册工具"走回调，而 native harness 没有这类工具。原注释称 MCP/select_tools 为 host-owned 属**文档错误，已改** |
| `stateRead`/`stateWrite` | **Rust 完全等价** | `StateStoreCallbacks` 以本地 `StateStore` 为准，宿主桥只是回退 |
| `checkpoint` | **Rust 完全等价** | 本地快照是权威 undo 锚，宿主锚是次要且 fail-open（`CHECKPOINT_UNSUPPORTED` 被识别） |

### 7.5 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib tools::bash_target_dirs` | 0 | 16 passed（含 pathe 三组、cd/pipeline/find/flag/去重/失败臂） |
| `cargo test --test bash_target_dirs_corpus` | 0 | **4 passed**：796 条全部与语法一致、17 条登记偏差恰为全集、宿主 14 条契约用例、预算臂（>10,000 节点 → `None`） |
| `cargo test --lib turn_loop::run_turn::tests::bash_operands` | 0 | 6 passed（无宿主解析器时原生生效、不可解析时保持 cwd-only、宿主覆盖仍被调用） |
| 变异验证 | — | 把 `run_turn` 的原生分支改成 `None` → `bash_operands_resolve_without_a_host_parser` 失败；还原即通过 |
| `cargo test --lib`（全量） | 0 | 3654 passed / 0 failed |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run typecheck` | 0 | clean |
| `bun run check:all` | 0 | 19 道全绿（architecture 2 指纹、normify 5 模块：core / core.turn-loop / napi / napi.bindings / tools；module-deps 271 边不变） |

**构建影响**：`kimi-agent` 新增 C 依赖（tree-sitter）。`--no-default-features --features cli`
的"纯 Rust 无 C 依赖"性质因此改变（`workflow-js` 早已引入 rquickjs），napi / Nix 打包面需在
下一次原生构建时复核。

## 8. 2026-11-06 补 Rust 之三：遥测事件的 v2 属性集（exit 0）

**落点**：`src/turn_loop/turn_step.rs`（`api_error`）、`src/turn_loop/run_turn.rs`
（`compaction_failed`）、`src/turn_loop/types.rs`（`LLM` 元信息访问器）、
`src/llm/http.rs` / `src/llm/multi.rs`（实现与委派）、`src/rpc/types.rs` +
`src/napi_bindings/params.rs` + `src/napi_bindings/turn.rs` + `src/config/mod.rs` +
`src/repl/mod.rs` + `src/server/engine.rs`（`alias` 字段贯通）、
`packages/node-sdk/src/native/native-llm-resolver.ts`（宿主侧带上 alias，含 pool 的
`nativeLlmWire` 显式映射）。

### 8.1 事实：两个事件都只发了 v2 声明集的子集

- v2 `ApiErrorEvent`（`app/telemetry/events.ts`）要求的属性本仓发了 4 个必填 + `step_no`，
  缺 `alias` / `provider_type` / `protocol` / `input_tokens` / `turn_id` / `request_kind`，
  且把可选属性 `request_id` **写成 `trace_id`**——消费者按 v2 名字取不到。
- v2 `CompactionFailedEvent` 的 `thinking_effort` 是**必填**，本仓此前以注释说明"故意缺失"
  （旧注释理由是"上下文不在 run_turn 里"）。这正是本轮标准下的未完成项。

### 8.2 改动

- `LLM` trait 新增 `model_alias` / `provider_type` / `protocol` / `thinking_effort`
  （默认 `None`）：`NativeHttpLlm` 从自身配置读，`MultiProviderLlm` 委派给首个 racer，
  host-proxy 保持 `None`。
- `NativeLlmConfig` / `JsNativeLlmConfig` / `ResolvedNativeLlm` 新增 `alias`，napi 映射、
  Rust 原生入口（repl/server）与宿主解析器（含 `[secondary_model]` 的显式 wire 映射）
  都填上——`ApiErrorEvent.alias` 因此有值可发，而不是补一个空字段。
- `api_error` 补齐：`alias`、`provider_type`、`protocol`、`request_kind: "turn"`、
  回合累计 `input_tokens`（`run_turn` 的 `total_usage` 传入 step，新增一个参数）、
  数值 `turn_id`（`turn_loop::turn_index` 从 `turn-N` / `subturn-N` 推出，其它形状省略）、
  `request_id`（由 `trace_id` 更名）。
- `compaction_failed` 补齐：`thinking_effort`（取当前 transport 的 effort，无 effort 时用
  wire 自己的 `none` 拼写）、数值 `turn_id`、摘要请求的 `request_id`（从
  `CompactionError::Provider` 内的 `LlmError` 取 trace id）。

### 8.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib turn_loop::turn_step` | 0 | 22 passed（`api_error` 断言新增 6 个字段） |
| `cargo test --lib overflow_compaction` | 0 | 10 passed（`compaction_failed` 断言 `thinking_effort`） |
| `cargo test --lib turn_loop::turn_index` | 0 | 1 passed（`turn-7`/`subturn-3` 映射、非法形状省略） |
| **变异验证 ×2** | — | 删 `thinking_effort` → 摘要失败用例红；删 `alias` → `api_error` 用例红；均按预期 |
| `cargo test --lib`（全量） | 0 | 3655 passed / 0 failed |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run typecheck` | 0 | clean |
| `bun run check:all` | 0 | 19 道全绿（architecture `kimi-agent`/`node-sdk`、normify 5 模块刷新） |

**未做**：`ApiErrorEvent.request_id` 只在 provider 回了 trace id 时出现（与 v2 的"响应头缺失
时省略"一致）；`compaction_failed` 的 `request_id` 同理。`telemetry` 事件本身不改语义——
本轮只补属性，不新增事件。

## 9. 2026-11-06 补 Rust 之四：hook.result 的 agent/turn 归属（exit 0）

**落点**：`packages/kimi-agent/src/tools/external_hooks.rs`、
`packages/kimi-agent/src/pipeline/mod.rs`、`packages/kimi-agent/src/callbacks.rs`、
`packages/kimi-agent/src/turn_loop/run_turn.rs`。

### 9.1 事实

v2 的 `HookResultPayload`（`features/externalHooks/agent/agentExternalHooksService.ts:47-53`）
要求 `agentId`，另有可选 `turnId`；其服务是 per-agent 的，因此每个结果天然带执行者身份。
本仓的 `hook.result` 事件此前只发 `hookEvent` / `content` / `blocked`——**没有 agent、没有
turn**。回合内子代理触发的 hook 输出因此只能被读成主代理的，与 v2 的行为不一致，也是
`HANDOFF` 台账里「hook payload 不带 agent id」那条的实处。

### 9.2 改动

- 新增 `HookResultMeta { agent_id, turn_id }`，`HookResultSink` 从
  `Fn(&str, &str, bool)` 改为 `Fn(&HookResultMeta, &str, &str, bool)`：身份随**每次触发**
  传递，而不是挂在 guard 上——同一会话的 guard 被主代理与子代理共用，挂在 guard 上正是
  v2 用 per-agent service 避免的那种串味。
- 逐条接上触发点：`denial`（PreToolUse，取 `ToolExecuteRequest` 的 agent/turn）、
  `notify_post_tool_use`（改为 `PostToolUse` 结构体传参，顺带解决 clippy 参数上限）、
  `run_user_prompt_submit`、`notify_stop` 各增 agent/turn 参数；`spawn_hooks` 接收 meta
  并随每个 hook 的异步任务克隆。会话级/观察型触发（SessionStart/End、PreCompact、
  Permission*、Subagent*、Task*）按定义属会话主代理，显式传 `MAIN_AGENT_ID` + 无 turn。
- `pipeline` 的 `hook.result` 事件新增 `agentId`、`turnId` 两个键。

### 9.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib external_hooks` | 0 | 37 passed（`denial_emits_a_hook_result_per_hook` 新增 agent/turn 断言） |
| **变异验证** | — | 把 `denial` 的 meta 换成空 agent → 该断言失败；还原即通过 |
| `cargo test --lib`（全量） | 0 | 3655 passed / 0 failed |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run check:all` | 0 | 19 道全绿（`kimi-agent` 指纹、normify `engine.core`/`core.turn-loop` 刷新） |

**未做**：`turnId` 在会话级触发上没有值（v2 的类型里它是可选的），因此省略而不是填一个
假 turn；子代理触发的 hook 走的是同一条 `denial` 路径，已天然带上子代理的 agent id。

## 10. 2026-11-06 补 Rust 之五：goal 的两笔旧账（exit 0）

**落点**：`packages/kimi-agent/src/goal/mod.rs`、
`packages/kimi-agent/src/rpc/types.rs`、`packages/kimi-agent/src/storage/state_store.rs`、
`packages/kimi-agent/src/napi_bindings/{params,turn}.rs`、
`packages/node-sdk/src/native/sdk-client/{build-handle,helpers}.ts`。

### 10.1 事实（回源核实，含推翻一条旧记录）

归档 §41 的「剩余」列了三项。逐项对着 `upstream/main` 核实后：

| 旧记录 | 核实结果 |
|---|---|
| `blocked_streak` 未实现（"v2 由 actor 运行时计数"） | **推翻**：`git grep blockedStreak upstream/main` 零命中，v2 的 `GoalState` / `GoalSnapshot` 都没有该字段。本仓的 `blocked_streak` 从未被写入、从未被读取，是 fork 自造的**死字段** |
| `GoalContext` 不带 `terminal_reason` | **属实**：v2 的 `GoalSnapshot.terminalReason` 是契约字段；本仓的宿主→引擎上下文没有它，于是 `sync_goal_from_context` 在宿主 pause 时把 `terminal_reason` 写成 `None`，宿主侧停机原因丢失 |
| SDK 无预算设置 RPC | **不是缺口**：v2 也只有模型工具 `SetGoalBudget`，没有宿主侧 RPC。维持现状即对齐 |

### 10.2 改动

- 删除 `blocked_streak`：`GoalSnapshot`、`GoalForModel`、`to_snapshot`/`goal_for_model` 的字段
  一并去掉（`GoalState` 本就没有）。它不在 v2 契约里，任何"补实现"都等于新增自创行为。
- `GoalContext` 增加 `terminal_reason`，一路贯通：`rpc/types.rs`（类型）→
  `napi_bindings/params.rs`（`JsGoalContext`）→ `napi_bindings/turn.rs`（映射）→
  宿主 `build-handle.ts` 的 `goal` 回调（发 `goal.terminalReason`）与
  `helpers.ts` 的 `NativeGoalState`（类型）。
- `sync_goal_from_context`：宿主把目标置为非 active 时，**保留宿主给出的原因**；目标回到
  active 时清空（运行中的目标没有 terminal reason）。

### 10.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib state_store` | 0 | 44 passed（同步用例新增：pause 带原因 → 存储保留；resume → 清空） |
| `cargo test --lib goal` | 0 | 153 passed（含 `test_snapshot_omits_absent_optionals`：wire 不再有 `blockedStreak`，`terminalReason` 仍在） |
| **变异验证** | — | 把同步改回 `terminal_reason = None` → 新断言失败；还原即绿 |
| `cargo test --lib`（全量） | 0 | 3655 passed / 0 failed |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run typecheck` | 0 | clean |
| `bun run check:all` | 0 | 19 道全绿（`kimi-agent`/`node-sdk` 指纹、normify 4 模块刷新） |

## 11. 2026-11-06 补 Rust 之六：undo 的 goal 尾巴（exit 0）

**落点**：`packages/kimi-agent/src/callbacks.rs`（`StateStoreCallbacks::checkpoint_state`）、
`packages/node-sdk/src/native/sdk-client/helpers.ts`（`goalStateFromEngineStore`、goal 映射）、
`packages/node-sdk/src/native/sdk-rpc-client-native.ts`（`undoHistory`）、
`packages/kimi-agent/napi-contract.d.ts`（重新生成）。

### 11.1 对齐判定（对着 TS 逐缝核实）

| 缝 | 判定 |
|---|---|
| `undoHistory` 的引擎回滚（`nativeRollbackEngineState` → `rollback_scoped_state`） | **Rust 已完全等价**：按 undo-anchor 回合数弹会话快照，栈耗尽静默停止 |
| undo 之后的 goal 投影 | **不是 Rust 单侧能收口的缺口**：宿主把 `meta.goal` 作为下一回合的 goal 提供者交给引擎，`sync_goal_from_context` 按 `updated_at` 信任更晚的宿主副本，于是引擎刚回滚掉的 goal 会被下一次回合原样写回——回滚的有效期只有一个回合 |
| checkpoint 的内容 | **Rust 侧的等价缺口（本批修）**：v2 的 undo 锚取自 goal 写入的同一个 store；本仓 goal 是 host-owned，靠回合末 fold 才落 store，回合头的锚看不到"用户刚建的 goal"，undo 会把 goal 删掉 |
| `plan` 域 | **已知偏差，不按缺陷处理**：plan 是 host-first（`stateRead` / `stateWrite` 由宿主回答），引擎回滚的 plan 快照是惰性的；undo 不回退 `planMode` 与计划文件。这是 M5 的设计决定，要改需另立裁决 |
| `todo` 域 | 不需处理：宿主每次 `getTodos` 都向引擎读 |

### 11.2 改动

1. `StateStoreCallbacks::checkpoint_state` 先折宿主 goal 再推快照：`Ok(Some)` → `sync_goal_from_context`；
   `Ok(None)` → `clear_host_goal`（只清 `host_owned`，引擎自建且宿主未采纳的不受影响）；
   `Err`（宿主没有该缝，如 standalone server / REPL）→ 一个字都不动。同步失败只 `warn`，不阻断锚点。
2. `goalStateFromEngineStore`：解码 store 的 `goal` 域。它与 `goal.updated` 的快照不同形——限额拼作
   `budgetLimits`，`wallClockMs` 是**已结算**值，续跑锚点 `wallClockResumedAt` 在旁边；宿主按 `updatedAt`
   外推（v2 `liveWallClockMs`），所以 active 目标要把 settled 值重锚到 now，否则每次都少算"上次 fold 到现在"。
   三态返回：`undefined` = 读不出来（保留宿主副本，绝不凭空造 state）、`null` = 存储里的"无 goal"。
3. `undoHistory`：rollback 消费了快照（`consumed > 0`）才采纳引擎回滚后的 goal。栈耗尽时引擎状态没变，
   宿主仍是权威，不动。只在 goal 的身份/计数真变时发 `goal.updated`——时间戳被重锚过，不能当变化判据。
4. `goalStateFromSnapshot` 补回 `terminalReason` / `completionCriterion`：此前两者被静默丢弃，
   宿主每次 `goal.updated` 都把引擎报的停机原因丢掉；`toGoalSnapshot` 同口径输出。

### 11.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib callbacks::tests::test_checkpoint_state` | 0 | 3 passed |
| **变异验证（sync）** | — | 把 `Ok(Some)` 分支的同步打掉 → `test_checkpoint_state_captures_the_host_goal` 红；还原即绿 |
| **变异验证（clear）** | — | 把 `Ok(None)` 分支改成空实现 → `test_checkpoint_state_follows_a_host_that_has_no_goal` 红；还原即绿 |
| `bun --bun run vitest run packages/node-sdk/test/goal-state.test.ts` | 0 | 9 passed |
| `bun --bun run vitest run packages/node-sdk/test/session-undo.test.ts` | 0 | 2 passed |
| **变异验证（读回）** | — | 关掉 undo 的读回 → 集成用例报 `expected 'complete' to be 'active'`；还原即绿 |
| `cargo test --lib`（全量） | 0 | 3658 passed / 0 failed |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run check:parity` | 0 | napi 104 / tools 89 / WS events 27 |
| `bun run typecheck` | 0 | 全部包 exit 0 |
| `bun run check:architecture` | 0 | 21 modules；指纹 `kimi-agent` / `node-sdk` 经 `--update` 刷新后复核 |
| `bun run check:normify` | 0 | 0 error / 60 warning；`napi.contract` / `sdk.node` / `sdk.node.native-client` 三个指纹经 `normify_module_refresh` 刷新 |
| `bun run scan:hardcoded:rust` | 0（第二次） | 首次失败：两条新 `warn` 文案未登记。按 `diagnostic` 逐行补进 `scripts/hardcoded-rust-allowlist.json`（+2 条，既有条目一字未动），复核 2729 recorded / 2729 found、0 new / 0 stale |
| `bun run check:all` | 0 | 20 道全绿（含上述扫描） |

### 11.4 说明与仍未收口

- 集成用例必须让模型在同一回合内 `UpdateGoal{status:'complete'}` 收尾：goal 处于 active 时引擎的泵会立刻续跑
  下一回合，`tryAcquireQuiescence()` 因此拒绝 undo（`SESSION_BUSY`）。这是既有语义，不是测试假象。
- `native-harness.test.ts` 的「installs a plugin…」用例在 `afterEach` 报
  `EBUSY: rm`（Windows 上目录仍被占用）。**与本节无关，属既有缺陷**：把本节唯一的 Rust 改动回退、重建插件后
  原样复现（`1 failed | 19 passed`），故不在此批处理，留作独立条目。
- `napi-contract.d.ts` 是构建产物：此前三次批次新增的字段注释（`terminalReason` / `alias` /
  `notifyUserEnabled`）没回写，本次构建补齐，纯注释变化，无签名变动。
- plan 的 undo 语义仍是开放项（见 §11.1 表格），与 §4 的待裁决清单一并跟踪。
- **`check:upstream-v2-delta` 的绿只代表"对该 pin 无新 delta"**：本机没有 `upstream` remote，且仓库是
  partial clone、promisor 远端走 HTTPS（github.com:443 在本机超时），`git fetch` 刷不动 ref，仍停在
  `a2afdbc7d`。要真正判定上游新增，需要先在能联网的机器上刷新该 ref 再复跑。

## 12. 2026-11-06 补 Rust 之七：undo 的可用性以 checkpoint 深度为准（exit 0）

**前提**：`.tmp/v2-ref` 与 `.tmp/v2-ref-upstream` 都不在本机。**已用本仓历史重建**
（`git archive ecad4136d9^ packages/{agent-core-v2,kap-server,klient,acp-server}` 解到 `.tmp/v2-ref`，
19 MB，全程无网络）；`.tmp/v2-ref-upstream` 仍缺 —— 它要 `upstream/main`，而本机无 `upstream` remote、
promisor 远端走 HTTPS 不通，见 §11.4。

### 12.1 v2 对照（`agent-core-v2/src/agent/undo/undoService.ts`）

| | v2 | 本仓（修前） |
|---|---|---|
| 可用性判据 | `availability()` = `min(computeUndoCut, checkpointDepth())`，`assertUndoAvailable` 在**切历史前**比对 | 只看 `computeUndoCut` |
| 栈不够时 | 拒绝：`SESSION_UNDO_UNAVAILABLE` + `reason: 'checkpoint_lost'`，文案 "Nothing to undo: conversation state checkpoints are incomplete"，什么都不动 | 照切 N 个回合，`rollback_scoped_state` 能弹几层弹几层 |
| 后果 | — | 转录回退 N 回合、todo/goal 等只回退 M<N 回合：面板与历史互相矛盾。fork 出来的会话（只复制转录、栈为空）是最常见的触发路径 |

### 12.2 改动

1. Rust `native_engine_state_checkpoint_depth(workspace_root, session_id)` → `StateStore::checkpoint_depth()`。
   宿主算不出这个目录（路径是规范化 workspace 的 digest），所以由引擎回答；会话级，不看 workspace 级栈。
2. TS `undoHistory`：取到 quiescence 之后、`setHistory` 之前读深度；`depth < cut.removedCount` →
   `SESSION_UNDO_UNAVAILABLE` / `checkpoint_lost`，`details` 带 `requestedCount` 与 `undoableCount`（v2 同字段）。
3. **手写转发层**：`packages/kimi-agent/index.native.cjs` 逐条转发 native 导出，新 `#[napi]` 不进去等于不存在
   （首次运行即 `nativeEngineStateCheckpointDepth is not a function`）。已补 `.cjs` 包装函数 + 导出表项，
   `index.native.d.ts` 的手写声明同步；`napi-contract.d.ts` 由构建重新生成。

### 12.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib native::native_tool_bindings` | 0 | 1 passed（0 → push 后 1 → rollback 后 0；另一会话互不影响） |
| `cargo test --lib`（全量） | 0 | 3659 passed / 0 failed |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun --bun run vitest run packages/node-sdk/test/session-undo.test.ts` | 0 | 3 passed（含 fork 场景 `checkpoint_lost`，且拒绝后转录一字未动） |
| **变异验证** | — | 把 `depth < cut.removedCount` 改成永不触发 → fork 用例转红；还原即绿 |
| `bun run typecheck` | 0 | 全部包 exit 0 |
| `bun run check:all` | 0 | 20 道全绿（`architecture` 指纹与 `napi` / `napi.contract` / `sdk.node` / `sdk.node.native-client` 四个 normify 模块经官方通道刷新） |
| `bun --bun run vitest run packages/node-sdk` | 1 | 447 passed / 1 failed —— 唯一失败是 §11.4 记录的既有 `native-harness` EBUSY，与本批无关 |

### 12.4 仍未收口

- **TUI `/undo` 菜单的可用性仍只按转录算**（`apps/kimi-code/src/tui/commands/undo.ts` 的
  `resolveUndoAvailability` 取 transcript 锚点与 context 锚点的较小值），不比 checkpoint 深度。v2 的
  `availability()` 取 `min(cut, checkpointDepth)`，所以 fork / 导入会话在菜单里仍会先给出选项、执行时才被拒。
  补齐需要 SDK 暴露一个可用性读取（转录 + 深度），属下一批。
- `packages/kimi-agent/test/surface-contract.test.ts` **在 HEAD 上就红**：`sessionTowerStatus` /
  `sessionTakeoverLock` 既不进手写转发层也不在 `UNWRAPPED_BINDINGS` 豁免表里（`357ffaea4` 引入；两者实际走
  `findKimiAgentAddon()` 的 raw addon 路径，`session-handle.ts` 与 `tower-status.ts` 直接 require）。
  与本节无关，未改动；要么补转发、要么按真实链路登记豁免，需单独一批。
- 本节新暴露的接缝值得记住：`#[napi]` 新导出有**三条**必经之路（Rust 属性、`index.native.cjs` 手写转发、
  手写 `index.native.d.ts`），`check:parity` 只覆盖第一条与生成契约之间的差集，第二条靠
  `surface-contract.test.ts` 兜，而那个套件当前是红的。
