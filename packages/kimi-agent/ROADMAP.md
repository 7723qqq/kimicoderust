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

## 13. 2026-11-06 补 Rust 之八：undo 的可用量接到 SDK 与 TUI（exit 0）

**来源**：§12 收口后留下的开口——v2 的 `availability()` 取 `min(computeUndoCut, checkpointDepth)`，
本仓的 TUI 只按转录锚点算（`resolveUndoAvailability` 取 transcript 与 context 的较小值），于是 fork /
导入会话会先给出可选条数、执行时才被 §12 的门禁拒掉。菜单与服务端的说法必须一致。

### 13.1 改动

1. `packages/node-sdk/src/types.ts`：新增 `UndoAvailability`（`{ maxTurns, stoppedAtCompaction }`，对齐 v2 字段名）。
2. `packages/node-sdk/src/rpc.ts`：基类 `getUndoAvailability` 抛 `NOT_IMPLEMENTED`（与其余 native-only 方法同形）。
3. `packages/node-sdk/src/native/sdk-rpc-client-native.ts`：`min(computeUndoCut(history, MAX_SAFE_INTEGER).removedCount,
   nativeEngineStateCheckpointDepth(...))`——与 `undoHistory` 实际执行的门禁同一对数。
4. `packages/node-sdk/src/session.ts`：`Session.getUndoAvailability()` 门面。
5. `apps/kimi-code/src/tui/commands/undo.ts`：`resolveUndoAvailability` 并入引擎上限；SDK 未实现时按鸭子类型跳过，
   保持原来两路取小的行为。

### 13.2 一处刻意的取舍（与 v2 的差异，已登记而非隐藏）

v2 的 `availability()` 是 `stoppedAtCompaction: cut.stoppedAtCompaction || maxTurns < cut.removedCount`——把
"栈比转录短"也折进这个布尔。本仓该字段驱动的是提示文案（"…after compaction"），照抄会让一个从未发生压缩的
会话被说成"受压缩阻断"。因此本仓只在**真撞压缩边界**时为真，栈不够只体现为 `maxTurns` 变小；拒绝时的 reason
仍按 v2 的词表区分（`checkpoint_lost` / `compaction_boundary`），用户仍能读到准确原因。

### 13.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `bun --bun run vitest run packages/node-sdk/test/session-undo.test.ts` | 0 | 3 passed（2 回合 → `maxTurns: 2`；fork → `maxTurns: 0`） |
| `bun --bun run vitest run apps/kimi-code/test/tui/commands/undo.test.ts` | 0 | 8 passed（引擎报 1 时 `/undo 2` 只出提示、不调 `undoHistory`；`/undo 1` 照常执行） |
| **变异验证（SDK）** | — | `maxTurns` 改回只看 `cut.removedCount` → fork 用例红（`{maxTurns:1}` vs 0）；还原即绿 |
| **变异验证（TUI）** | — | `maxCount` 不再与引擎取小 → 新用例红；还原即绿 |
| `bun --bun run vitest run apps/kimi-code/test/tui/commands` | 0 | 253 passed / 21 files |
| `bun --bun run vitest run packages/node-sdk/test/native-unimplemented.test.ts` | 0 | 4 passed（基类抛 `NOT_IMPLEMENTED` 的既有契约不破） |
| `bun run typecheck` | 0 | 全部包 exit 0 |
| `cargo fmt --check` / `cargo clippy --all-targets --features cli -- -D warnings` | 0 / 0 | clean（本批无 Rust 改动，仍照跑） |
| `cargo test --lib` | 0 | 3659 passed / 0 failed |
| `bun run check:all` | 0 | 20 道全绿；`node-sdk` 指纹与 `sdk.node` / `sdk.node.native-client` 两个 normify 模块经官方通道刷新 |

### 13.4 仍未收口

- `packages/kimi-agent/test/surface-contract.test.ts` 仍红（`sessionTowerStatus` / `sessionTakeoverLock`
  未转发也未登记豁免，见 §12.4），与本批无关。
- `packages/kimi-agent` 的 undo 语义剩下 plan 一侧：undo 不回退 `planMode` 与计划文件（host-first 设计决定，
  §11.1 表格），需裁决后才能动。

## 14. 2026-11-06 补 Rust 之九：工具结果的 note 贯通三层（exit 0）

**来源**：v2 `renderToolResultForModel`（`agent-core-v2/src/agent/contextMemory/toolResultRender.ts`）
在**模型可见**的工具结果上做三件事：状态包装、墙钟头、`content + '\n' + note`。本仓的前两件早已移植
（`tool_result_render::render_status` + `wall_time::prepend_wall_time`，顺序与 v2 相同），第三件也在
`run_turn` 里——但 `tool_result_render.rs` 的头注释写着"note 追加**没有**移植"，这句错话掩盖了一个真实缺陷。

### 14.1 事实：`note` 被两种用途共用

| 用途 | 位置 | 后果 |
|---|---|---|
| 给人/模型看的散文（v2 `ToolResult.note`） | Read 的续读提示与编码告示、readMedia 的视频说明 | 正确用途 |
| 内部来源标签 | `tool_policy`、`tool_select`、`native_subagent`、`native_team`、`mcp:<server>` | 因为 `run_turn` 无条件追加，**每一次 MCP 调用、每一次被拒调用**的模型结果末尾都多一行标签 |

这些标签没有任何读取方：宿主 SDK 过去根本不转发 `note`，全仓 TS 侧对这几个字面量零命中，属死写入。

### 14.2 三层改动

1. **引擎**：删掉 10 处标签写入（`tools/dispatch.rs` 2、`tools/subagent_tools.rs` 4、`tools/team_tool.rs` 2、
   `mcp/manager.rs` 2），`note` 从此只有散文一个含义；`tool_result_render.rs` 与 `turn_loop/types.rs` 的字段文档
   按实际行为重写。
2. **协议**：`ToolResultEvent` 增可选 `note`（`packages/protocol/src/events.ts` 的类型与 zod schema 同步），
   语义写明是"引擎追加进模型结果的那段散文"。
3. **宿主**：SDK 的 `tool.native` 映射透传 `note`（空串转 `undefined`）；TUI 的 `handleToolResult` 按 v2 顺序
   把它接在 `output` 之后，卡片因此显示模型看到的那段提示（此前用户看不到自己模型被告知了什么）。

### 14.3 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib tools::tests` | 0 | 247 passed（含新增：被 `[tools]` 禁用的调用 `note.is_none()`；延后加载的 MCP 调用与真正执行的 MCP 结果都 `note.is_none()`） |
| **变异验证（标签）** | — | 把 `tool_policy` 标签加回去 → 新用例红（`note = Some("tool_policy")`）；把 `mcp:{server}` 加回去 → 延后测试红（`note = Some("mcp:github")`）；两处还原即绿 |
| `bun --bun run vitest run packages/node-sdk/test/native-harness.test.ts -t "carries the engine note"` | 0 | 1 passed：真实引擎 + mock LLM 让模型 Read 一个 3000 字符长行 → `tool.result.note` 含 `were truncated` / `use Bash`，且 `output` 里不含它 |
| **变异验证（宿主）** | — | 删掉 SDK 的 `note` 透传 → 该用例红（`expected [] to have a length of 1`）；还原即绿 |
| `bun --bun run vitest run apps/kimi-code/test/tui/controllers/session-event-handler-plugin-updates.test.ts` | 0 | 8 passed（有 note 时卡片 output 为 `内容\nnote`；无 note 时保持原样） |
| `bun run typecheck` | 0 | 全部包 exit 0 |
| `cargo test --lib` / `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 | 见 §14.4 的实测数字 |
| `bun run check:all` | 0 | 见 §14.4 |

### 14.4 收口实测

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib`（全量） | 0 | 3660 passed / 0 failed（`mcp::manager::tests` 的五处标签断言改为 `note.is_none()`） |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run typecheck` | 0 | 全部包 exit 0 |
| `bun --bun run vitest run apps/kimi-code/test/tui/controllers` | 0 | 412 passed / 22 files |
| `bun --bun run vitest run packages/node-sdk` | 1 | 448 passed / 1 failed —— 唯一失败仍是 §11.4 记录的既有 `native-harness` EBUSY（与本批无关） |
| `bun run check:all` | 0 | 20 道全绿；`architecture` 指纹经 `--update` 刷新，`engine.mcp` / `engine.mcp.manager` / `engine.tools.agent` / `engine.tools.team` 四个 normify 模块经 `normify_module_refresh` 刷新 |
| 原生插件 | 0 | `bun run build`（release）重建，测试跑在新二进制上 |

### 14.5 复核：卡片"缺状态包装与墙钟头"是误判（已关闭）

初稿把"卡片只有 `output + note`、没有 `<system>…</system>` 包装与 `Wall time:` 头"记成缺口，回 `.tmp/v2-ref`
核实后**推翻**：

| 事实 | 证据 |
|---|---|
| v2 的循环事件只带**原始** output 与 note，不带渲染文本、不带墙钟 | `agent-core-v2/src/agent/loop/loopService.ts` 的 `appendLoopEvent({ type: 'tool.result', result: { output, isError, note } })` |
| 状态包装只发生在**面向模型**的投影里 | `agent/contextProjector/projection.ts` 的 `projectedContent`：`role === 'tool'` 时才调 `renderToolResultForModel({ output, isError, note })` |
| 宿主侧转换直接透传原始 output（字符串原样、对象 JSON 化） | `acp-server/src/convert.ts` 的 `toolResultToAcpContent` |
| 墙钟头不在冻结参考里 | `.tmp/v2-ref`（`ecad4136d9^`）全仓 `Wall time` 零命中；本仓 `wall_time.rs` 注明它来自上游 **#3966**，晚于该快照 |

也就是说：宿主看到"原始 output + note"、模型看到"包装 + 墙钟 + note"，两侧都是 v2 的形态，本仓现在与之一致；
卡片显示 note 恰好对应 v2 转录里那条消息（`loopEventFold` 把 note 存在工具消息上，投影时再拼接）。

**参考时效提醒**：`.tmp/v2-ref` 冻结在 2026-09-08 的删除点，晚于它的上游改动（如 #3966）在本地无法逐字核实；
涉及这类条目要么等 `v2-ref-upstream` 可用，要么以本仓台账里已登记的上游 sha 为准，不能默认"参考里没有就是没有"。

## 15. 2026-11-06 插件注册表所有权重写：不再持有 SQLite 句柄（exit 0，EBUSY 未闭环）

**来源**：`packages/node-sdk/test/native-harness.test.ts` 的「installs a plugin…」在 `afterEach` 报
`EBUSY: rm`，§11.4 做过差分（回退实现照样复现）确认是既有缺陷。本轮按要求**重写所有权**而不是继续追 `Arc`
持有者。

### 15.1 事实与重写

`PluginManager` 直接持有 `Arc<SqliteSessionStore>`，即一条活着的连接、钉在 `<data dir>/sessions.db` 上。
`close_plugin_store()` 只把全局置空，任何克隆到 manager 的地方都会让这条连接活到进程结束；Windows 上就是
数据目录删不掉。

改法：manager 只存**路径**，每次插件 API 调用开一个 store、用完即 drop。

| 位置 | 改动 |
|---|---|
| `session/sqlite_store.rs` | `SqliteSessionStore` 记录 `path: Option<PathBuf>` 并提供 `path()`；`open` 记录、`in_memory` 为 `None` |
| `server/plugins.rs` | `store: Arc<…>` → `registry: RegistryStore`（`File(PathBuf)` / `Memory(Arc<…>)`）；读写改为 `read_state` / `write_state`，文件型每次开闭 |
| `server/plugins/tests.rs` | 新增用例：写入后换新连接仍读得到（证明落盘），manager 丢弃后文件可删 |

`Memory` 分支保留句柄是因为内存库没有文件可重开——测试用的就是它。

### 15.2 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib` | 0 | 3661 passed / 0 failed（+1 新用例） |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean |
| `bun run check:all` | 0 | 20 道全绿；`architecture` 指纹与 `core.session` / `core.session.store` / `plugins` / `plugins.registry` 四个 normify 模块经官方通道刷新 |
| 原生插件 | 0 | 重建（release） |

### 15.3 EBUSY 仍未闭环（如实记录）

重写后**这条用例仍然红**，且残留文件仍是 `agent/sessions.db`。已排除的部分：

- `initPluginStore` + `closePluginStore` 单独调用时，DB 文件立即可删（探针实测），说明注册表这条路已经不再持句柄；
- 失败后进程表里没有残留 node 子进程，锁在 vitest worker 自身。

仍未知的：napi 路径上还有谁打开 `<data dir>/sessions.db`。全仓 `SqliteSessionStore::open` 的调用点只有
`main.rs`（CLI / standalone server）与 `init_plugin_store`，而本用例走的是 napi；结合"加入 `createSession`
才出现"，怀疑点转向会话建立/销毁过程里对同一路径的第二次打开（90 KB 的库里有会话表，不像只有注册表 blob）。

下一步需要一次带插桩的复现：在 worker 内 `harness.close()` 之后逐个尝试删除 `agent/` 下的条目并打印成功/失败，
以定位到具体文件与打开时机；本轮的探针在"建会话 + close"这一步会挂住，说明那里本身还有一个未闭环的等待，
需与 EBUSY 一并查。

## 16. 2026-11-06 v2 → Rust 完整移植的分步账（第 1 步：`state` 已对账）

### 16.1 先建账的原因

实测（`.tmp/v2-ref`，排除 `*.test.ts` / `*.d.ts`）：v2 非测试源码 **1021 个文件**、16 个子系统 + 4 个根文件；
本仓 `packages/kimi-agent/src` 374 个 `.rs`、约 1.05M 字符。

先做了一次符号级扫描（v2 文件的导出名在 Rust 里出现即算命中）：**592 个文件零命中**。但绝大多数是 DI 服务壳
（`XService` / `IXService`），行为在 Rust 里换了名字实现——例如活动面板 → `server/activity.rs`，上下文记忆 →
`session/` + `storage/` + `native/event_store/loop_fold.rs`。**符号命中与否都不是证据**，这正是"100% Rust 化"
一直没人能逐文件核实的原因：能核实的是"行为对位 + 钉住它的测试"，不是名字。

### 16.2 分步法（一个子系统一步）

每步四条验收：

1. 列出该子系统全部 v2 源文件（非测试）；
2. 每个文件写出 Rust 对位（路径，或"无"）+ 钉住它的**测试名**；
3. 没有对位的文件 → 立为任务，或按 `not-applicable` 记录并写明理由（DI 装配、immer 基础设施等）；
4. 该步收尾时子系统内每个文件都有归属，缺口进 §4 清单。

规模（现读，非测试文件数）：`agent` 253、`app` 211、`features` 201、`session` 74、`_base` 67、`kosong` 62、
`workspace` 54、`os` 17、`mcpCore` 16、`persistence` 13、`wire` 12、`tool` 10、`runtime` 9、`debug` 9、
`state` 7、`program` 2、根 4。

顺序按"用户可见影响 × 能否独立验证"：**state → tool → wire → session → agent（loop / contextMemory / goal /
permission）→ mcpCore → kosong → workspace → features → app → 基础设施**。

### 16.3 第 1 步：`state`（7 文件，已对账）

| v2 文件 | Rust 对位 | 结论 |
|---|---|---|
| `state.ts` — immer 补丁式状态框架：`replayable`/`undoable` key、fold 注册表、`FoldContext`（checkpoint / clearCheckpoints / undoToCheckpoint） | `storage/state_store.rs`（域快照 + checkpoint 栈）、`native/event_store/loop_fold.rs`（事件→状态折叠） | **等价替换**，机制不同（整域快照 vs 补丁）。两条不变量无 Rust 对应：`state.duplicate_fold`、`state.durability_mismatch`——本仓域集合是固定表（`STATE_DOMAINS`），重复折叠与瞬态/持久错配构造不出来 |
| `agentModel.ts` — 域资源运行时 + `AgentModelBridge` | `session/mod.rs` 的持久状态 + `storage/state_store.rs` 的会话域 | 等价替换：无桥接层，域由引擎直管 |
| `agentEffect.ts` — `defineAgentEffect` DI 工厂 | 无 | `not-applicable`：DI 装配，Rust 无容器 |
| `errors.ts` — `state.*` 三个错误码 | 无 | 两个不可达（见上）；`state.cycle` 是补丁回放的环检测，本仓无补丁回放 → `not-applicable` |
| `eventDispatcher.ts` — dispatcher 接口、`RestorePhase`、`DurableAgentRuntimeParticipant` | `native/event_store/`（append / fold / replay）；undo 深度见 §12 | 接口层 `not-applicable`；`RestorePhase` **待核**（见 16.4） |
| `eventDispatcherService.ts`（30 KB：补丁记录、`checkpointDepth`、恢复、flush） | `native/event_store/` + `storage/state_store.rs` + `session/mod.rs` | 行为面已覆盖（checkpoint 深度 §12、undo §6/§7）；补丁级重放以整域快照替代 |
| `stateContribution.ts` — fold 注册的 contribution 集合 | 无 | `not-applicable`：DI 贡献点 |

### 16.4 第 1 步产出的待核项（第 2 步的第一件事）

- **`RestorePhase`（`new|restoring|ready|failed`）在 Rust 没有对应物**。要核的不是"少个枚举"，而是 v2 用它
  **挡住恢复未完成时的动作**（会话重放期间不接受新回合/工具调用）。本仓 `resumeSession` 从 SQLite 同步读历史、
  状态域按需读取，可能根本不存在那个窗口——这与 `state/` 是否等价直接相关，必须读 `session/mod.rs` 的恢复路径
  判定，不能以"同步所以没问题"结案。
- `_base/state/stateRegistry`（key 注册与快照排除 `snapshotExcluded`）只以 `StateKey` 类型被 `state/` 引用，
  归 `_base` 那一步一起对账，本步不重复计。

## 17. 2026-11-06 分步移植第 2 步：`tool`（10 文件）对账 + spill 豁免补齐（exit 0）

### 17.1 先关掉第 1 步的待核项：`RestorePhase` → not-applicable

v2 的 `RestorePhase`（`new|restoring|ready|failed`）并不挡用户动作，它挡的是**构造顺序**：

| v2 用法（`state/eventDispatcherService.ts`） | 语义 |
|---|---|
| `restore()` 只允许在 `new` 阶段调用 | 恢复只能做一次 |
| 恢复开始后 `contribute` 新的 replayable state 直接抛错 | 状态必须在恢复前注册 |
| 恢复开始后 `attach` durable participant 抛错，`ready` 之后才允许 `attachLate` | 运行期参与者必须在恢复前或恢复后接线，不能卡在中间 |

这三点都是 v2 的 DI / actor 装配不变量；本仓没有"贡献-恢复"生命周期：`resumeSession` 注册 meta → 建句柄 → 挂历史，
在句柄就绪之前 `prompt` 与 `undoHistory` 都以 `session.not_found` 拒绝（`prompt` 的 `!meta.handle` 分支，
`sdk-rpc-client-native.ts`），不会出现"半个会话"被使用。**结论：行为面 not-applicable**。
残留一条小事：那句错误文案对"正在恢复的会话"说成 "unknown or closed"，措辞不准，已记，不单独成批。

### 17.2 `tool/` 逐文件对账（10 文件）

| v2 文件 | Rust 对位 | 结论 |
|---|---|---|
| `toolContract.ts`（契约：50k 字符、10M 保留、`note`/`spill`/`truncated`） | `turn_loop/types.rs::ExecutableToolResult`、`tool_result_truncation.rs`（常量同值：`DEFAULT_TOOL_RESULT_MAX_CHARS = 50_000`、`DEFAULT_TOOL_RESULT_MAX_RETAINED_CHARS = 10_000_000`） | 等价；`spillExempt` 原本缺（见 17.3，本轮补上） |
| `result-builder.ts`（50k/2000/`[...truncated]`/``Output is truncated to fit in the message.``） | `native/output_truncate.rs` + `tool_result_truncation.rs` | **等价且更底层**：v2 的这个 builder 本身就把分块逻辑交给 native（`tryNativeWriteToolOutputChunk`），Rust 就是那个 native 实现 |
| `output-accumulator.ts`（流式累积 + 保留上限 + spill） | `tool_result_truncation.rs`（spill/指针/10M 保留） | 等价 |
| `native-glob-match.ts`（调 native addon） | `native/glob.rs` | 被取代（本仓就是 addon） |
| `input-schema.ts`（zod → JSON Schema） | `tools/core_tool_defs.rs` 手写 schema | `not-applicable`：本仓直接以 JSON Schema 定义，无 zod |
| `args-validator.ts`（Ajv 校验入参） | 无中心校验；各工具自行校验（`arg_error` / `engine.tools.argMustBe*`） | **部分等价**：非法入参两侧都变成错误结果，但 v2 用 schema 拦截、报错文案来自 Ajv，本仓是逐工具手写。记录，不冒充等价 |
| `tool-args-parse.ts`（`parseToolCallArguments`：坏 JSON → `parseFailed` 并报给模型） | `llm/openai.rs:847` 等传输层：坏参数**丢弃该 tool call**，不报错 | **不等价，见 17.4**（本轮只记录，未改） |
| `path-access.ts`（路径分类/敏感文件/规范化/盘符桥接） | `permission/`、`native/tool_access.rs`、`native/shell_path*` | 等价（早期批次已逐条对过） |
| `rule-match.ts`（权限路径规则匹配） | `permission/` 规则匹配、`native/permission` 的 `nativeParsePermissionPattern` | 等价 |
| `toolInputDisplay.ts`（12 变体显示联合） | `tool_input_display.rs` | 等价（`1879104` 生成 ACP golden 时逐字段移植） |

### 17.3 本轮修：读回 spill 文件不再被二次截断（v2 `spillExempt`）

v2 的 `readTool` 在"读的路径本身就是 spill 文件"时给结果打 `spillExempt`，`toolResultTruncationService`
见到就直接原样返回。缺了它，模型读回一个被 spill 的大文件会被再次截断+再次 spill——拿到的是"指针的指针"，
它要的尾部永远读不到。

改法（一处判定 + 一处调用）：

- `ToolResultTruncator::is_spill_read(tool_name, args)`：仅 `Read`，且请求路径规范化后落在 `spill_dir` 内；
- `ToolResultTruncator::truncate_call(args, request)`：命中豁免则原样返回（`truncated: false`、`spill_path: None`），
  否则走原有策略；
- `callbacks.rs` 的截断点改用 `truncate_call`（那里 `request.tool_name` 与 `request.arguments` 都在手边）。

验证：

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib tool_result_truncation` | 0 | 8 passed（新增：读回 spill 文件原样返回；普通文件仍被截断并落盘） |
| **变异验证** | — | 把 `is_spill_read` 改成恒 false → 新用例红（内容被二次截断）；还原即绿 |

### 17.4 本轮查出、尚未修的一处（`tool/` + `wire` 交界）

坏的工具调用参数两侧处理不同：v2 解析失败会带回 `parseFailed` 并让模型看到一个"参数不是合法 JSON"的错误结果，
可以自我纠正；本仓传输层直接把该 tool call **丢弃**（`llm/openai.rs` 的 `filter_map` 注释写明"不伪造空参数调用"）。
丢弃至少不会产生悬空 tool_call id，但模型得不到任何反馈，可能重试或自说自话。

修法需要把"解析失败的调用"保留进 assistant 消息并合成一条错误 tool 结果——跨 turn loop 与传输层，属独立一批；
本轮只记录，不半做。

## 18. 2026-11-06 分步移植第 3 步：`wire`（5 文件）对账（exit 0，无代码改动）

### 18.1 先纠正一个计数

§16.2 的 `wire` 12 个文件含测试（`*.test.ts`）。**非测试只有 5 个**：`record.ts`、`repair.ts`、`wire.ts`、
`wireService.ts`、`errors.ts`，共 532 行。其余子系统的数字同样含测试，逐子系统对账时以该步现读的非测试清单为准。

### 18.2 逐文件对账

| v2 文件 | Rust 对位 | 结论 |
|---|---|---|
| `record.ts` — 记录形状、`wire.jsonl` 键、parts 变换/脱水 | `wire_events` 行（`id`/`event_type`/`payload`/`is_checkpoint`/`is_compaction`/`protocol_version`）+ `turn_loop/types.rs` 的消息类型 | **底座不同、目的等价**：v2 是逐 agent 的 jsonl 追加日志，本仓是一张 SQLite 表；协议版本列与拒绝更新版本的逻辑见 `bc4b157` |
| `repair.ts` — 尾巴被撕裂时的修复：备份 `.bak`、重写存活记录、上报丢弃条数 | 无 | `not-applicable`：SQLite 事务写入没有"半行"这种状态，该失败模式在本仓不存在 |
| `wire.ts` / `wireService.ts` — append / readJournal / flush | `native/event_store/mod.rs`（`append_event`、`read_fold_rows`、`fold_projection`）+ `session/sqlite_store.rs` | 等价：追加、按 seq 折叠、压缩边界（`is_compaction` 之后的 seq 起点）都在 |
| `errors.ts` — `wire.*` 错误码 | 版本闸门的错误路径（`WireProtocolTooNew`）等 | 仅"可达的错误"需要码；`wire.duplicate_op` 这类对应 v2 的 DI/装配检查，本仓无该结构 → `not-applicable` |

### 18.3 对账查出的两处（诊断面，非行为，未改代码）

| 差异 | v2 | 本仓 | 影响 |
|---|---|---|---|
| 未知/损坏记录的上报 | `reportSkippedRecord(..., malformed)` → `onUnexpectedError`，带 `WIRE_UNKNOWN_RECORD` 与条数 | **坏 JSON 的 payload** 会跳过并计数（有用例钉住）；**未知 `event_type`** 走 `match` 的 `_ => {}`，静默丢弃 | 只影响可观测性：历史会少几条消息而无任何诊断。模型看到的是"少了几条"，不是错误 |
| 被跳过条数的去处 | 上报 | `read_fold_rows` 返回 `(rows, dropped)`，`fold_projection` 用 `.0` 把 `dropped` 丢掉 | 同上 |

两处都属于"日志/上报"层面的等价，不改变模型可见行为；补它们只需要在 `fold_projection` 里对被丢弃计数与未知类型各打一条 `warn`。
本轮**不写无测试覆盖的日志改动**，登记为下一步的可选项。

### 18.4 把 §17.4 的"坏参数被丢弃"量到可执行

`ToolCall` 是 `turn_loop/types.rs` 的结构体，全仓构造点约 200 处（其中 121 处在 `tool_scheduler` 的测试里）。
两条候选修法：

| 方案 | 做法 | 代价 / 风险 |
|---|---|---|
| A（推荐） | `ToolCall` 增 `args_parse_error: Option<String>`；传输层解析失败时**保留该调用**并带上原因；turn loop 见到它就不再执行工具，直接合成一条错误 tool 结果；assistant 消息保留该 call id | 结构体字段会让 ~200 个字面量需要补一个字段（绝大多数是测试，机械可改）；协议序列化要确保该字段不发给上游 provider |
| B | 保持丢弃，另发明一条"无对应 call"的 tool 结果 | **不可取**：严格协议（Anthropic 的 `tool_use`/`tool_result` 必须成对）会拒绝；等于用一个新 bug 换旧 bug |

边界条件（写进 A 的验收）：流式参数被截断、参数不是对象、`arguments` 为空串、provider 返回非流式坏 JSON —— 四种都要
变成"一条可读的错误 tool 结果"，并有用例钉住其中至少三种。属独立一批（跨 `llm/*` 与 `turn_loop`），不在本步半做。

## 19. 2026-11-06 补 Rust 之十：坏参数的工具调用保留并回给模型（exit 0）

**来源**：§17.4 / §18.4。**落点**：`src/llm/tool_args.rs`（新）、`src/turn_loop/types.rs`、
`src/llm/{openai,anthropic,openai_responses}.rs`、`src/turn_loop/run_turn.rs`、
`src/turn_loop/run_turn/tests/basic_loop.rs`、`packages/i18n-catalog/src/locales/{en,zh}.ts`，
以及 148 处 `ToolCall` 字面量。

### 19.1 回源修正：v2 并没有一条"参数不是合法 JSON"的模型文案

§17.4 把 v2 的行为写成"带回 `parseFailed` 并让模型看到一个'参数不是合法 JSON'的错误结果"。回源后两处要改：

| v2 事实 | 位置 |
|---|---|
| `parseToolCallArguments` 失败返回 `{ data: {}, parseFailed: true, error }`，**调用不丢** | `packages/agent-core-v2/src/tool/tool-args-parse.ts:12-20` |
| `preflightToolCall` 只把 `parseFailed` 写 debug 日志，随后拿 `{}` 去跑 Ajv；模型读到的是 `Invalid args for tool "<name>": <ajv error>` | `packages/agent-core-v2/src/agent/toolExecutor/toolExecutorService.ts:738-746`、`:778-786` |
| 也就是说，`{}` 若恰好能满足 schema（全参数可选的工具），v2 **会照常执行** | 同上（`kind: 'runnable'` 分支） |

所以对齐点是"保留调用 + 不丢反馈 + 不执行假参数"，不是抄一句 v2 不存在的文案。本仓没有 Ajv，逐工具校验的措辞
也各不相同，这一点记为**已登记偏差**（见 19.4）。

### 19.2 本仓修前三套互斥行为

| 路径 | 修前 | 后果 |
|---|---|---|
| openai 流式（`StreamAccumulator::finish`） | `filter_map` 丢掉解析失败的调用 | 模型零反馈，只能盲重试 |
| openai 非流式（`parse_response`） | `unwrap_or_else(|_| json!({}))` | 带空参数**真的去执行**（Bash/Write 无输入） |
| `openai_responses` 流式与非流式 | 整条响应 `?` / 置 `error` | 一个坏调用杀死整轮，模型无从纠正 |
| anthropic 流式 | `if let Ok(...)` 丢弃 | 同 openai 流式 |

### 19.3 改动

1. `ToolCall.args_parse_error: Option<String>`，`#[serde(skip)]`——引擎内部判定，不上线格式；线格式仍只有
   `arguments`（已解码值）。字段落地的实际代价：编译器报 **148 处**字面量缺字段（§18.4 估的 243 把
   `StreamDelta::ToolCall` / `StreamKind::ToolCall` 一并算进去了），按编译器位置逐处补齐。
2. `llm::tool_args::parse_tool_arguments`（v2 同名函数的逐分支移植）：缺省、`""` → `{}` 且不失败；
   合法 JSON → 原样；解析失败 → `{}` + serde 的原因。四条传输路径统一走它，`openai` 非流式的"非字符串
   `arguments`"分支按 v2 `typeof raw !== 'string'` 保留原值。
3. `run_turn` 预检（v2 `preflightToolCall`）：`arguments: null` 先归一成 `{}`（v2 的 null 分支）；
   解析失败或解出的不是对象 → 从可执行集摘出，逐条补一条 `role: "tool"` 错误结果。assistant 消息在这之前
   已经带上全部调用，所以 `tool_use`/`tool_result` 的配对不破。
4. 文案走引擎 i18n，落在既有 `Invalid <tool> arguments: …` 家族：`engine.tools.argsNotJson`
   （`{{tool}}` + `{{detail}}`）、`engine.tools.argsNotObject`（`{{tool}}`）。`{{detail}}` 是 serde 原文，
   与 v2 把 provider 错误文本原样带上是同一口径。

### 19.4 已登记的行为偏差

- **v2 在 `{}` 能通过 schema 时会执行，本仓一律拒执行。** 本仓没有中心 schema 校验，逐工具校验对"全参数可选"
  的工具不成立，执行 `{}` 会退化成"以空参数运行 Bash/Write"。因此选更窄的一侧：拒绝并提供可读原因。
  这条写进了 `unusable_tool_arguments` 的注释，不是默认行为。
- **预检拒绝的调用不发 `tool.native` 事件**，与既有 swarm 拒绝同形（那条路径也只推消息）。模型看得到失败，
  TUI 卡片看不到；v2 两种拒绝都走 `tool.result` 通道。单列一条，不在本批扩。

### 19.5 验证

| 检查 | 出口码 | 输出 |
|---|---:|---|
| `cargo test --lib --features cli -- llm::` | 0 | 267 passed（含 decoder 3 条 + 三条传输路径改写后的 4 条） |
| `cargo test --lib --features cli -- a_call_with_unusable_arguments_is_answered_not_executed` | 0 | 1 passed（0.01s；坏 JSON 与非对象两条调用各得一条 tool 结果，assistant 仍带两个 `tool_use`，回合继续到第 2 步） |
| **变异验证（预检）** | — | `unusable_tool_arguments` 恒 `None` → 5.01s 超时失败，信息为"an unrunnable call must be answered, not dispatched to the host" |
| **变异验证（传输层）** | — | openai 流式把 `ToolCall.args_parse_error` 换成 `None` → `truncated_tool_call_arguments_keep_the_call_and_its_reason` 红 |
| `cargo test --lib --features cli`（全量） | 0 | **3666 passed / 0 failed**（26.4s） |
| `cargo clippy --all-targets --features cli -- -D warnings` / `cargo fmt --check` | 0 / 0 | clean（clippy 先报了 `redundant_guards`，改成 `Some("")` 分支后清） |
| `bun run typecheck` | 0 | 全部包 exit 0 |
| `bun run check:all` | 0 | 20 道全绿：`architecture` 指纹 `kimi-agent` / `i18n-catalog` 刷新；normify 14 模块刷新；`scan:hardcoded:rust` 2729 → **2727**（两条被删掉的 fail-closed 文案按 stale 清除，无新增） |

**测试设计上的一处修正**：变异第一次不是"失败"而是"挂住"——预检摘掉后坏调用真的被派发给测试替身宿主，
而那个 RPC 替身没有处理器。挂起是红灯但不可作回归信号，因此 `run_turn` 外面套了 5 秒 `tokio::time::timeout`，
失败信息直接说明"本应被回答，而不是派发给会阻塞的宿主"。

**下一步（已由用户裁决）**：Rust TUI 的基座整体换成 `textual-rs`（MIT，0.3.16，ratatui 0.30 + crossterm 0.29
后端，自带 headless `TestApp`），本仓只保留 `EntryOp`/`EntryId` 契约与投影；`kimi-ui-core` 的组件树、
`kimi-ui-terminal` 的 ANSI 后端、`kimi-tui-app` 的 crossterm 循环随之退役。本机取包需
`CARGO_HTTP_CHECK_REVOKE=false`（schannel 吊销检查离线失败），registry 已是 rsproxy。
