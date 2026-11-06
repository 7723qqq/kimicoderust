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
- **provider 环境变量未进共享锁**：`packages/kimi-agent/src/config/tests.rs` 里
  `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` / `KIMI_BASE_URL` 仍是裸 `set_var`；同模块独占这些
  变量，跨文件窗口不成立，但形状与已修的两处一致。
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
