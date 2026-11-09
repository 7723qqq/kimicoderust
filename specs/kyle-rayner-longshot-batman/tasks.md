# 任务

每个阶段独立可交付、可回滚。所有阶段的通用验收（不再逐条重复）：

- `cargo check --lib --features cli`（在 `packages/kimi-agent`）通过。
- `cargo test --lib --features cli` 通过，且通过数不低于拆分前基线。
- `bun run check:all` 退出码 0。
- `napi-integration.test.ts` 通过（napi 外壳未破）。

## T0 协调（阻塞所有后续阶段）

工作树有并行会话的 332 个未提交文件，P0 会改到 `llm/`、`turn_loop/`、`rpc/`、`callbacks` 的 import。

- [ ] T0.1 与并行会话对齐：要么等它收口，要么在独立 worktree 基于 `main` HEAD 开工。
- 验收：`git status --short packages/kimi-agent` 不再同时出现「对方未提交改动」与「本拆分改动」的重叠文件。

## P0 `kimi-core`

- [ ] T0.1 新建 `packages/kimi-core`（Cargo workspace 成员），把叶子模块搬入：`primitives`、`env`、`turn_events`、`frontmatter`、`tool_input_display`、`tool_result_truncation`、`goal`、`knowledge`、`cron`。
  验收：`cargo check -p kimi-core` 通过；`cargo check --lib --features cli`（kimi-agent）通过。
- [ ] T0.2 把 `turn_loop/types.rs` 的 wire/LLM 类型与 `rpc/types.rs` 的 `TokenUsage` 等下沉到 `kimi-core::types`，全仓 `use crate::turn_loop::types::…` / `use crate::rpc::types::…` 改为 `kimi_core::types::…`。
  验收：`cargo check --lib --features cli` 通过；`bun run check:rust-module-deps -- --update` 后 `llm -> turn_loop` 与 `rpc -> turn_loop` 两类边消失。
- [ ] T0.3 下沉 `MAIN_AGENT_ID`（callbacks）、`EventBus`（events）、`is_system_reminder`（injection）、`PolicySnapshot`（permission）、`SandboxMode`（tools）、`SummaryPolicy`（subagent）到 core。
  验收：`cargo check --lib --features cli` 通过；`rpc -> callbacks`、`events -> callbacks` 边消失。
- [ ] T0.4 全量验证。
  验收：`cargo test --lib --features cli` 全绿且通过数 ≥ 基线；`bun run check:all` 退出码 0；热重编实测（touch `llm/` 一个文件）≤ 20s。

## P1 `kimi-host`

- [ ] T1.1 抽出 `HostCallbacks` trait 与 `RpcServer` trait 到 `packages/kimi-host`；`callbacks` 里 `RpcHostCallbacks` 对具体 `RpcServer` 的依赖改为 trait。
  验收：`cargo check --lib --features cli` 通过；`callbacks -> rpc::server::RpcServer` 的具体类型边消失。

## P2 `kimi-rpc` + `kimi-permission`

- [ ] T2.1 抽 `kimi-rpc`（`rpc/types.rs` + `rpc/server.rs`），依赖 core + host。
  验收：`cargo check -p kimi-rpc` 通过；`kimi-agent` 侧 `use crate::rpc::…` 改为 `kimi_rpc::…`。
- [ ] T2.2 抽 `kimi-permission`，依赖 core。
  验收：`cargo check -p kimi-permission` 通过。

## P3 `kimi-storage`

- [ ] T3.1 把 `server/files.rs`（`FileStore`）下沉到 `kimi-storage`；`tools/file_ops.rs` 改用 trait。
  验收：`cargo check --lib --features cli` 通过；`tools -> server` 边消失。
- [ ] T3.2 抽 `storage` + `session` 到 `kimi-storage`，引入 `UploadCache` trait 供 `llm/media_resolver.rs` 使用。
  验收：`cargo check --lib --features cli` 通过；`llm -> session` 边消失。

## P4 `kimi-llm`

- [ ] T4.1 抽 `llm` 到 `packages/kimi-llm`，依赖 core/host/rpc。
  验收：`cargo check -p kimi-llm` 通过；`kimi-agent` 侧 `use crate::llm::…` 改为 `kimi_llm::…`；`llm -> turn_loop`、`llm -> injection`、`llm -> callbacks` 边消失。

## P5 `kimi-tools`

- [ ] T5.1 抽 `tools`，依赖 core/host/llm/permission/storage。
  验收：`cargo check -p kimi-tools` 通过；`tools -> server`、`tools -> config`、`tools -> injection` 边消失。

## P6 `kimi-turn-loop`

- [ ] T6.1 抽 `turn_loop`，依赖 core/host/llm/tools/permission/storage。
  验收：`cargo check -p kimi-turn-loop` 通过；`turn_loop -> callbacks`、`turn_loop -> compaction`、`turn_loop -> injection`、`turn_loop -> llm` 边消失。

## P7 `kimi-server`

- [ ] T7.1 把剩余模块（`server` `acp` `repl` `napi_bindings` `config` `pipeline` `injection` `prompt` `compaction` `subagent` `swarm` `mcp` `skills` `callbacks`(impl) `events`(impl) 及小模块）归入 `kimi-server`；`kimi-agent` 只留 `lib.rs` re-export 与 `main.rs`。
  验收：`cargo check --lib --features cli` 通过；`cargo test --lib --features cli` 全绿；`napi-integration.test.ts` 通过；对外路径 `kimi_agent::…` 不变（grep 现有 `packages/node-sdk/src` 与 `apps/*/src` 的引用，无改动）。

## 收尾

- [ ] T8.1 更新 `architecture.json`（新 crate 集合与依赖边）、`scripts/rust-module-deps-baseline.json`（重新记录）、`normify-kimi-code/`（引擎模块指纹）。
  验收：`bun run check:architecture`、`bun run check:rust-module-deps`、`bun run check:normify` 全绿。
- [ ] T8.2 热重编实测记录：改动 `kimi-llm` 一个文件、`kimi-tools` 一个文件、`kimi-server` 一个文件各自的总耗时。
  验收：均显著低于 43.8s，且 `kimi-llm` 的改动不触发 `kimi-server` 重编（依赖方向正确）。
