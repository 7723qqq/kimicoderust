# 开发行为规范

本文件把根 `AGENTS.md` 的强制短条款展开为可执行流程。每节均给出规则、仓库内依据、操作方法和完成判据；行号是 2026-11-04 撰写时快照，后续定位应优先使用章节、符号和命令名。

「已核实」表示能在仓库文件、脚本或事故记录中直接定位；「规范归纳」表示本次把多处既有事实统一成操作流程，不冒充仓库已有原文。

<a id="ledger-no-rollback"></a>
## 1. 累积型台账不可整文件回滚

**规则**

`scripts/*allowlist*.json`、`architecture.json`、`normify-kimi-code/**`、`scripts/roadmap-citation-anchors.json` 等承载累积记录的文件，禁止用 `git checkout --`、`git restore` 或整文件覆盖做手工回滚；只能逐行编辑，或先备份再做可审计的增量修改。官方刷新命令只能按第 2 节执行。

**为什么（已核实）**

- `HANDOVER-kimirust.md` §7.1（第 158 行）记录了真实事故：B-1#11 批次对 `hardcoded-rust-allowlist.json` 执行一次 `git checkout --`，丢失一整天台账累积，最后逐行手工重建。
- `scripts/hardcoded-rust-allowlist.json` 第 2 行声明它是两向 ratchet：新增未记录字面量会失败，已记录但消失或迁移的条目也会失败；它不是可随意重建的缓存。
- `scripts/check-architecture-drift.mjs` 第 24—29、777—788 行说明 `--update` 会重算指纹并整文件写回 `architecture.json`，所以并发写入和整文件回滚都会覆盖不属于当前批次的记录。
- `normify-kimi-code/modules/kimi-code/engine/server/http.md` 第 13—16 行同时保存 `revision`、`fingerprint` 和 `source`，属于持续维护的结构数据，不是一次性说明文字。

**怎么做**

1. 写入前读取目标文件最新内容，并检查它的工作区状态：

```powershell
git status --short -- scripts/hardcoded-rust-allowlist.json architecture.json normify-kimi-code
```

2. 需要回退自己的改动时，先备份到 `.tmp/`，再用逐行 `Edit` 删除或恢复本批条目；不得对共享文件执行 `git checkout --`、`git restore`、`git stash` 或整文件 `Write`。
3. 用路径限定的 diff 复核，不看无关文件，也不替别人清理改动：

```powershell
git diff -- scripts/hardcoded-rust-allowlist.json
```

**怎么判定做对了**

- diff 只包含本批目标条目，其他会话和其他批次的记录保持原样。
- JSON 仍可解析，条目键仍唯一；结构数据仍通过对应的只读门禁。
- 回退后有逐行 diff 或备份对照，能够解释每一处删除，而不是“文件看起来恢复了”。

<a id="single-writer-updates"></a>
## 2. 刷新类命令单点收口

**规则**

`check:architecture:update`、`check:rust-module-deps:update`、`scan:hardcoded:rust -- --update`、`normify_module_refresh` 等写入型刷新，只能由一个会话在所有并行写入停止后执行一次；执行前备份或记录哈希，执行后逐项对账，再运行对应的只读门禁。

**为什么（已核实）**

- `package.json` 第 61—67 行给出真实入口：`check:architecture:update` 调用 `check-architecture-drift.mjs --update`，`check:rust-module-deps:update` 调用 `check-rust-module-deps.mjs --update`，`scan:hardcoded:rust` 调用 `scan-hardcoded-rust.mjs`。
- `MEMORY.md` 第 96—119 行给出 `normify_module_refresh` 的真实 CLI 形式，并记录子模块和父模块必须一起刷新，否则父指纹仍然过期。
- `SPLIT-PLAN-kimi-agent.md` 第 146、358 行记录了并行会话正在修改台账或 `architecture.json` 时禁止本批刷新、必须等待落盘。
- 同文件第 405—410 行留下了正确做法：刷新前保存 `.tmp/architecture.before.json`，刷新后用 `git diff --no-index` 确认只有目标指纹一行变化。

**怎么做（规范归纳）**

1. 明确唯一执行会话，确认其他写入窗口已经停止或完成交接；仍有并行写入时不得刷新。
2. 备份目标文件并记录刷新前哈希。以 `architecture.json` 为例：

```powershell
Copy-Item architecture.json .tmp/architecture.before.json
Get-FileHash architecture.json -Algorithm SHA256
bun run check:architecture:update
$updateExit = $LASTEXITCODE
Get-FileHash architecture.json -Algorithm SHA256
git diff --no-index -- .tmp/architecture.before.json architecture.json
```

`git diff --no-index` 在有差异时返回 1，这是 diff 语义，不是刷新门禁出口码；刷新成败看已保存的 `$updateExit`。

3. Normify 使用实际模块 ID，并按漂移输出同时指定子模块与必要祖先：

```powershell
bun packages/normify/src/cli.ts normify_module_refresh '{"project":"kimi-code","repoRoot":".","ids":["<child>","<parent>"]}'
bun run check:normify
```

4. 对 `hardcoded-rust-allowlist.json` 的官方更新结果逐条人工分类和复核；不得把自动写入当作语义审查。

**怎么判定做对了**

- 同一刷新目标只有一个执行者，执行前没有未停止的并行写入。
- 前后备份或哈希齐全，diff 只出现预期模块、指纹、revision 或目标条目，没有整文件格式化潮和条目丢失。
- `$updateExit` 为 0，随后的只读门禁也为 0；两者不能互相替代。

<a id="file-ownership"></a>
## 3. 文件所有权与并行窗口

**规则**

同一时间一个窗口只修改自己声明的文件域；开工前先用 `git status --short` 核对占用，同名文件必须让行或取得明确交接。接手半截改动时，先读 diff、批前记录和已有验证，判断它是否有明确归属、完整意图和可继续验证的边界；无法判断就停止并交接，不得默认回滚。

**为什么（已核实）**

- `HANDOVER-kimirust.md` §9（第 180—187 行）列出了并行会话的实际占用文件，并要求接手前运行 `git status --short`；同名改动必须让行，或确认对方收口、明确交接后再动。
- `SPLIT-PLAN-kimi-agent.md` 第 273、293、358、426 行分别记录了等待在飞改动落盘、先恢复基线、等待并行写入以及因禁碰台账而中止批次。
- `MEMORY.md` 第 89—94 行记录了错误工作目录下的 sparse-checkout 操作重新物化文件、静默复活另一任务未提交删除的事故；不了解中间态就“恢复”会破坏他人工作。

**怎么做**

1. 开工和收口各记录一次状态，并在本窗口任务说明中列出文件域：

```powershell
git status --short
git diff -- <本窗口声明的路径>
```

2. 发现目标文件已有改动时，先确认归属和交接状态；若获准接手，读完 diff、相关备份、上一批验证输出，再判断能否在原状态上继续。
3. 归属、意图或可验证边界任一不明时停止修改；不得用 `restore`、`checkout`、`stash` 或覆盖写入制造一个“干净”起点。

**怎么判定做对了**

- 本窗口 diff 只落在声明文件域，其他窗口占用文件没有被修改、格式化或回滚。
- 半截改动有明确的“继续、交回或经授权中止”结论；继续时能指出上一状态和本批新增边界。
- 没有以“我不知道这些改动来自哪里”为理由清理工作区。

<a id="verification-tiers"></a>
## 4. 验证优先级与失败归因

**规则**

先用能观察本次改动的最窄静态门禁和定向测试，交付前再跑一次全量回归验收；测试失败先做未改动 HEAD 或拆分前后对照，再窄化复现，区分既有失败、环境性失败和本批回归，不得靠忽略、跳过、放宽断言或提高预算换绿。

**为什么（已核实）**

- `AGENTS.md` “Verification tiers”（第 465—503 行）和 “Verification Standard”（第 800—818 行）要求按 blast radius 分层，全量验证放在交付前，失败后只重跑失败项。
- `MEMORY.md` 第 215—240 行记录了 `cargo check --lib` 不编译集成测试和 `#[cfg(test)]` 的假绿灯，并给出迭代用 `cargo check --all-targets`、提交边界跑全量的分级。
- `package.json` 第 39—67 行是 Bun 门禁命令的权威清单；`.github/workflows/ci.yml` 第 110—146 行是 Rust fmt、clippy、构建和测试的实际 CI 命令。
- `MEMORY.md` 第 25—49 行记录了修改前 main 已红、单独运行会变绿的负载相关失败；这支持基线对照和孤立复跑，而不是直接改实现或重跑全套。

**怎么做**

Rust 迭代门禁在 `packages/kimi-agent` 下执行：

```powershell
cargo fmt --check
cargo clippy --all-targets --features cli -- -D warnings
cargo check --all-targets --features cli
cargo test --lib <改动模块或已知消费方模块>
```

TypeScript 和结构门禁只选本次 diff 能触达的项：

```powershell
bun run typecheck
bun run lint
bun run check:architecture
bun run check:normify
bun run check:rust-module-deps
bun run check:parity
```

交付前按 `AGENTS.md` 的 Tier 4 做全量验收；CI 的 Rust 全量命令是：

```powershell
cargo test --no-default-features --features cli,workflow-js
```

失败时先固定同一个失败测试和同一命令。必要时用独立、干净的 worktree 在未改动 HEAD 上复现，禁止在当前工作区回滚别人改动来制造基线：

```powershell
git worktree add --detach <baseline-dir> HEAD
```

在 `<baseline-dir>` 运行同一个定向测试。HEAD 同红只能证明“既有”，不能单独证明“环境性”；还要用单独复跑、平台差异或资源争用证据归因。HEAD 绿而当前红时优先按本批回归排查。拆分任务另按第 7 节做拆分前后对账。

**怎么判定做对了**

- 每条实际执行的门禁都有真实出口码，执行范围与改动面相符，全量测试只作为最终验收而非逐次编辑的探测手段。
- 失败结论同时给出基线结果和定向复现结果；未做对照时只写“未归因”，不写“环境原因”。
- 没有新增 `#[ignore]`、条件跳过、断言放松、重试或超时预算膨胀来掩盖失败。

<a id="batch-closure"></a>
## 5. 批次收口定义

**规则**

一个批次只有同时完成源码核实、相关门禁真实出口码为 0、台账同步、无半截状态、未经授权不提交 commit，才算收口；缺一项就必须保留为未完成并明确报告。

**为什么（已核实）**

- `FS-LIST-PROTOCOL.md` 第 135—145 行要求每批独立收口，硬要求包括 `check:all`、clippy、typecheck 出口码为 0、台账逐行同步和不提交 commit。
- `HANDOVER-kimirust.md` 第 160—177 行把源码复核、判定测试、门禁出口码、全量回归边界和未验证项报告连成同一套约束。
- `AGENTS.md` “Working Principles” 第 796 行禁止 stub、空实现和只满足签名的假完成。
- `SPLIT-PLAN-kimi-agent.md` 第 397—412 行是完整收口样例：列出命令出口码、台账刷新差异、未执行操作和未验证范围。

**怎么做**

按固定清单逐项关闭：

1. 源码依据能定位到文件、符号、行或上游 sha，不用台账 note 代替源码。
2. 相关静态门禁和定向测试逐条记录命令与真实出口码；未运行就写“未运行”。
3. 同步本批拥有的 allowlist、ROADMAP、RUST-PORT-GAP 或结构数据，其他条目不碰。
4. 用 `git status --short` 和路径限定 diff 检查没有临时文件、半截实现、遗忘的调试探针或他人文件改动。
5. 没有用户明确授权就不 commit；已获授权的 commit 才继续遵循仓库既有的 hook 与 push 规则。

**怎么判定做对了**

- 批次报告能逐项回答“源码证据、出口码、台账、工作区、commit”五件事，没有一句笼统的“已完成”替代证据。
- 不能关闭的项明确标为阻塞或未完成，不留无归属的中间态，也不通过自行 commit 制造完成感。

<a id="upstream-delta"></a>
## 6. 上游 delta 纪律

**规则**

处理上游 delta 时必须用 `git show upstream/main:<path>` 或 `git grep` 回到上游源码核实，不信任 allowlist note；只修改目标 sha 的单条记录，禁止用 `--update` 全量重扫代替分诊，verdict 或标签变化必须同批同步 ROADMAP 与 RUST-PORT-GAP 的对应项。

**为什么（已核实）**

- `scripts/check-upstream-v2-delta.mjs` 第 12—30、47 行定义了 ratchet 和合法 verdict：`ported`、`tracked`、`not-applicable`、`pending`；`pending` 仍失败。
- 同脚本第 126—144 行证明 `--update` 会按当前上游历史重建整个 entries 列表，因此它不是单条分诊工具。
- `HANDOVER-kimirust.md` 第 160—167 行记录了 note 失真事故、verdict 三件套以及本地读上游源码的命令。
- `FS-LIST-PROTOCOL.md` 第 156—162 行给出同步实例：目标 sha 的 allowlist entry、`RUST-PORT-GAP.md` 对应行、architecture 和 parity 同批更新。
- `AGENTS.md` 第 746—751 行说明该门禁只覆盖新 commit，绿灯不等于移植完整。

**怎么做**

```powershell
git show upstream/main:<path>
git grep <pattern> upstream/main -- <path>
git log -1 --format=%H upstream/main
```

需要刷新引用且网络可用时，先按根 `AGENTS.md` 执行：

```powershell
git fetch upstream main:refs/remotes/upstream/main --force
```

随后只读目标 entry，并用逐行编辑完成以下内容：

- verdict 只能是脚本接受的四个值；完成分诊不得保留 `pending`。
- note 写明上游 sha、fork 落点 `file:line`、判定测试名和已核实偏差，不复制旧结论。
- 同批更新 `packages/kimi-agent/ROADMAP.md` §6 与 `RUST-PORT-GAP.md` 的对应项；若目标 sha 在其中一处没有对应项，写明实际归属，不编造交叉引用。
- 不运行 `bun scripts/check-upstream-v2-delta.mjs --update`，不顺手改其他 sha。

完成后只跑只读门禁：

```powershell
bun run check:upstream-v2-delta
bun run check:roadmap-refs
```

**怎么判定做对了**

- diff 只触及目标 sha 及确有对应关系的台账行，上游证据命令可以复现同一结论。
- `ported`、`tracked`、`not-applicable` 与实际代码和缺口状态一致；note 中的文件和测试能被 `check:roadmap-refs` 解析。
- 上游 ref 不可解析时出口码 2 被如实视为未验证，不能写成绿灯。

<a id="pure-moves"></a>
## 7. 大文件拆分只能纯搬运

**规则**

大文件拆分只移动代码，不改逻辑；项级多重集必须 `missing=0`、`extra=0`，测试数量与函数名必须一一对齐，适配仅限模块声明、模块内引用路径和最小可见性 `pub(super)`。

**为什么（已核实）**

- `SPLIT-PLAN-kimi-agent.md` 第 269—292 行明确纯搬运边界、共享 helper、测试名保留、可见性和模块依赖约束。
- 同文件第 317—330 行要求拆分前后运行 `cargo test --lib <module> -- --list`，并用 `git diff --color-moved=blocks` 提供纯搬运证据。
- 同文件第 392—395 行记录了实际验收：测试函数名、顺序、断言未改，顶层项 `missing/extra=0`，去空白逐项文本和字符串字面量均 `mismatch=0`。
- 同文件第 421—431 行说明即使测试和多项门禁变绿，扫描器暴露新字面量后批次仍不能算完成。

**怎么做**

1. 拆分前把原文件备份到 `.tmp/`，导出顶层项清单和测试列表；拆分后用同一提取口径再导一次。
2. 项级对账使用“种类＋名称＋归一化正文”的多重集，不仅比较总数；输出 `missing`、`extra`、正文 `mismatch` 和字符串字面量 `mismatch`。临时对账脚本放 `.tmp/`，不进入业务源码。
3. 测试列表按函数名多重集和总数同时比较，模块路径变化不计为函数改名：

```powershell
cargo test --lib <module> -- --list
```

4. 只补 `#[cfg(test)] mod ...;`、模块内引用和必要的 `pub(super)`；共享 helper 保持单一实例，测试属性、函数顺序和断言随项整体移动。
5. 人工复核移动块，并运行受影响门禁：

```powershell
git diff --color-moved=blocks --color-moved-ws=allow-indentation-change
bun run check:rust-module-deps
bun run check:roadmap-refs
bun run scan:hardcoded:rust
```

**怎么判定做对了**

- 项级 `missing=0`、`extra=0`、正文 `mismatch=0`、字符串字面量 `mismatch=0`，测试数量和函数名多重集完全一致。
- diff 中除模块层级、必要引用和 `pub(super)` 外没有逻辑改写，没有趁机重命名或调整断言。
- 拆分域定向测试和相关静态门禁均为 0；任何新暴露的扫描项都单独处理，不用更新豁免直接吞掉。

<a id="gate-blindspots"></a>
## 8. 门禁自身可被质疑

**规则**

绿灯只证明门禁实际覆盖的输入和路径没有报错；发现扫描截断、错误码或行为语义盲区、测试跳过、CI 漏收时，必须另立盲区或修复条目，禁止通过扩大 allowlist 豁免换绿。

**为什么（已核实）**

- `SPLIT-PLAN-kimi-agent.md` 第 425 行记录了 `scan-hardcoded-rust.mjs` 在第一个 `#[cfg(test)]` 处截断整个文件，导致后续产品文本长期未扫描，且控制台把剩余条目折叠为 `and 6 more`。
- `AGENTS.md` 第 414—426 行说明 TypeScript 与 Rust hardcoded 扫描覆盖互不相交；第 746—751 行说明 upstream ratchet 只按新 commit 键控。
- `RUST-PORT-GAP.md` 第 57—60 行明确 parity 只比界面，不比默认值、错误处理、时序和生命周期。
- `.github/workflows/ci.yml` 第 137—146 行记录集成测试在 CLI binary 缺失时会跳过并通过，因此 CI 另加 binary 存在性断言；第 148—166 行说明根 vitest 不收 `pi-tui`，必须独立 job。

**怎么做**

1. 新盲区单独记录：门禁及版本或 commit、声明覆盖、实际覆盖、漏掉的最小案例、复现命令、临时风险边界和修复去向。
2. 行为或移植缺口进入 `RUST-PORT-GAP.md` 等既有缺口台账；扫描器或 CI 自身缺陷另列修复项，不把“工具没看见”写成“代码不存在问题”。
3. 先构造应当失败的负例验证门禁能否看见，再修扫描器、CI 或代码；`MEMORY.md` 第 244—259 行的变异验证记录了“逻辑被测、接线没被测”的同类问题。
4. 已有 allowlist 只表示记录债务，不为新暴露项批量添加豁免来恢复绿灯。

**怎么判定做对了**

- 盲区有独立条目和最小负例，修复后同一负例会红、正确实现会绿。
- 报告准确写“某个门禁的某项覆盖通过”，不把单一绿灯扩大成“全部行为已验证”。
- 没有为了消化新暴露问题而扩大豁免；确实需要豁免的项逐条有语义理由和归属。

<a id="live-counts"></a>
## 9. 文档数字必须现读现算

**规则**

声称当前状态的 ported、tracked、not-applicable（简写 na）、pending、测试数、模块数、文件数和行数，必须现读权威台账或命令输出并注明统计口径；禁止从旧文档抄入手写快照。带日期的历史事故数字可以保留，但不得冒充当前值。

**为什么（已核实）**

- `AGENTS.md` 第 43 行要求版本不要 hardcode，第 296、309、647 行分别要求 slash command、CLI subcommand 和 vitest project 读取实时清单。
- 同文件第 83—91 行自陈 i18n 叶子数是无人门禁校验的手工快照，需要重新推导；`DEVELOPMENT-OPTIMIZATION.md` 第 51—54 行记录该数字已经过期。
- `RUST-PORT-GAP.md` 第 47—55 行把统计与当轮门禁或脚本输出逐项对应，体现了“数字必须带证据”的口径。

**怎么做**

上游 verdict 分布直接读取 allowlist：

```powershell
bun -e 'const x=await Bun.file("scripts/upstream-v2-delta-allowlist.json").json(); console.log(x.entries.reduce((a,e)=>(a[e.verdict]=(a[e.verdict]??0)+1,a),{}))'
```

架构模块数直接读取模型：

```powershell
bun -e 'const x=await Bun.file("architecture.json").json(); console.log(x.modules.length)'
```

测试清单使用真实 runner 输出，不数旧文档中的名称：

```powershell
cargo test --lib <module> -- --list
```

引用数字时同时记录命令、统计范围和必要的 ref 或日期；verdict 分类总数应与 entries 总数对账，`na` 必须在正文中展开为 `not-applicable`。

**怎么判定做对了**

- 读者能用同一命令、同一范围重算出数字，当前值没有无来源的手工常量。
- 不把不同口径相加，例如简单属性计数与 cargo 实际发现测试数、serde rename 与业务字段数不得混用。
- 历史数字明确标注日期或 commit，当前数字明确标注实时来源。

<a id="exit-codes"></a>
## 10. PowerShell 必须读取真实出口码

**规则**

PowerShell 中门禁必须直接运行并立即读取 `$LASTEXITCODE`；不得用管道末端状态或后台 shell 的返回值替代被测命令出口码。

**为什么（已核实）**

- `HANDOVER-kimirust.md` 第 164 行记录了 `bun run X | Select-Object` 会让管道末端命令决定状态，要求直跑并读取 `$LASTEXITCODE`。
- `MEMORY.md` 第 237—240 行记录了后台任务只拿到 shell 状态、把红色测试误读为绿色的事故。

**怎么做**

需要保存日志时，重定向输出但仍直跑原命令，并先保存出口码再读取日志：

```powershell
bun run check:all > .tmp/check-all.log 2>&1
$gateExit = $LASTEXITCODE
Get-Content .tmp/check-all.log
if ($gateExit -ne 0) { throw "check:all failed with exit code $gateExit" }
```

后台包装必须显式传递被测命令的出口码；不能把启动后台任务成功、日志文件存在或管道成功当作门禁通过。

**怎么判定做对了**

- 报告中的出口码来自被测命令本身，日志结论与 `$LASTEXITCODE` 一致。
- 失败命令不会被后续 `Get-Content`、管道或后台启动成功覆盖成 0。
- `git diff --no-index` 等以 1 表示“有差异”的工具单独解释，不与门禁出口码混为一谈。

<a id="patch-budget"></a>
## 11. 补丁代码行不得超过第一方源码的 0.05%

**规则**

补丁只能通过 `packages/**/src/**/patches/**` 目录结构界定，不能用注释、标记文本或提交说明界定。分子是该路径下的生产源码行，分母是 `packages/**` 下的第一方 `.rs` / `.ts` 生产源码行；两者都按非空、非注释的物理行计数，Rust 的 `#[cfg(test)]` 项不计入。补丁行数不得超过 `floor(分母 × 0.05%)`。按 2026-11-04 实测基线 209,117 行换算，绝对额度为 104 行；该数字只说明当前量级，实际额度始终以脚本实时统计为准。补丁是例外手段，不是常规实现方式。

**为什么（已核实）**

- `scripts/check-no-comments.mjs` 第 5—7、90—130 行会在 `packages/transcript` 的 `src/`、`test/`、`scripts/` 中直接拒绝注释；注释不是全仓库统一可用的补丁标记机制，预算元数据也不能侵入产品源码。
- `package.json` 的 `check:all` 当前没有补丁预算项，因此阶段一必须显式运行独立命令；没有执行记录时，不能把其他门禁绿灯当成预算已验证。
- `scripts/check-patch-budget.mjs` 以路径和词法扫描计数，包含工作区未提交、未跟踪的源码，避免尚未暂存的补丁逃过测量。

**怎么做**

1. 需要隔离补丁时，只使用上述 `patches/` 路径，不在源码中添加预算标记注释，也不新建平行命名来绕开统计。
2. 直接运行独立门禁并保存真实出口码；在接入 `check:all` 前不得省略这一步：

```powershell
bun run check:patch-budget
$budgetExit = $LASTEXITCODE
```

3. 核对输出中的分母、分子、比例、绝对行数上限、每包分布，以及生成文件、测试文件、构建目录和内联测试项的排除清单。分母排除 `test/`、`tests/`、`__tests__`、测试专用文件、`target/`、依赖与构建产物、NAPI 生成声明；`apps/`、`reports/`、`normify-kimi-code/` 和 `scripts/` 不在 `packages/**` 口径内。
4. 超限时缩减补丁或调整实现边界；不得通过删除其他源码、扩大排除项或改变分子分母口径换绿。

**怎么判定做对了**

- `check-patch-budget` 输出 `OK`，真实出口码为 0，且 `Patch code lines` 不大于输出的 `Patch budget`。
- 输出比例不超过 0.05%，绝对上限使用向下取整；超限时脚本输出 `FAIL` 并以非零出口码结束。
- 每包分布之和等于分母，补丁只由约定路径贡献；排除清单能解释测试、生成产物和构建目录为何不计数。
- 没有注释标记、口径绕行或未声明的目录例外，也没有把尚未接入的 `check:all` 绿灯冒充本项验证。

## 核实边界与本次归纳

以下内容与正文规则分开记录，避免把推断写成仓库既有事实：

- “单点收口”的统一措辞是本次规范归纳；已核实依据是刷新脚本会整文件写入、并行修改时必须等待的既有记录，以及拆分批次的前备份、后 diff 实例。
- “可继承中间态”的三项判断是本次操作化定义；仓库已有的是并行占用、明确交接、禁碰文件和错误恢复造成破坏的记录，没有现成的同名 checklist。
- HEAD 独立 worktree 对照是本次操作化流程；仓库已有“先复现再修”、修改前 main 已红和定向复跑记录，但未找到统一的 HEAD 对照命令模板。
- 未找到“CI 漏收新 crate 测试”的直接记录，因此正文没有把它写成既发事故；正文只使用已核实的 CLI binary 缺失跳过、`pi-tui` 独立 job、扫描截断和语义盲区。
- 未找到 `scripts/.hardcoded-allowlist.lock` 的仓库内约定或实现，因此本规范不虚构该锁路径；共享文件安全由单点刷新、文件所有权和逐行编辑约束保证。
