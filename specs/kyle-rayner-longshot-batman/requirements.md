# 拆分 `packages/kimi-agent` 为多个 workspace crate

## 目标

当前 `packages/kimi-agent` 是一个约 100k 行的单 crate（449 个 `.rs`、44 个顶层模块）。任何文件变动都会让这个 crate 整体重编，实测热重编 **43.8s**（`cargo build --lib --features cli`，touch 一个文件），测试目标更久。

本次拆分要把它拆成若干个 workspace crate，使**一次改动只重编落在的那个 crate 及其下游**，把热重编从 40s 级降到 10s 级（改动落在叶子/中层 crate 时）。

## 受众

本仓开发者（人与其他 agent）。拆分后 `packages/kimi-agent` 仍是 napi/cdylib 的产出方，对外接口（`kimi_agent::…`、`kimi-agent` 包）不变。

## 范围内

- 把 `packages/kimi-agent/src` 的顶层模块按层拆成新的 workspace crate。
- 为打破循环依赖而做的**依赖倒置**（把类型下沉、把具体类型换成 trait）。
- 构建配置（`Cargo.toml`、workspace 成员、`.cargo/config.toml`）。
- 门禁同步：`architecture.json`、`scripts/rust-module-deps-baseline.json`、`normify-kimi-code/`。

## 范围外（明确不做）

- **任何行为变更**。拆分是纯结构搬迁 + 依赖倒置，模型可见行为、wire 格式、事件词汇、错误码都不变。
- human 模块缺口补全（另一条线，已在 `.tmp/human-parity-reconciliation.md` 记账）。
- TypeScript 侧（`packages/node-sdk`、`apps/*`）的改动，除了为适配新 crate 边界而必须的最小改动。
- CI 工作流重写（保持现有 job 结构，只让它们能构建新的 crate 集合）。

## 验收（可检查）

1. `cargo check --lib --features cli` 通过，`cargo test --lib --features cli` 全绿（与拆分前同一基线）。
2. 新 crate 之间的依赖图**无环**（`architecture.json` 的 `Check B` 能验证）。
3. 热重编实测：改动一个中层 crate 的文件后，`cargo build` 的耗时显著低于 43.8s（目标 ≤ 20s）。
4. `bun run check:all` 全绿（含 `check:architecture`、`check:rust-module-deps`、`check:parity`）。
5. napi 产物仍能被 `packages/node-sdk` 加载（`napi-integration.test.ts` 通过）。

## 已知风险

- 依赖图是一个 **30 模块的强连通分量**（生产代码），拆分必须先打破循环，否则搬不动。打破循环要引入 trait，可能改变若干模块的公开类型。
- 并行会话正在同一工作树改 `llm/`、`turn_loop/` 等模块（332 个未提交文件）。**拆分期间必须避免与它抢同一批文件**，否则两边互相覆盖。
