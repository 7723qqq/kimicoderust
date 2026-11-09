# 设计：分层 crate + 依赖倒置

## 现状（实测，2026-11-10）

用仓库自带的 `scripts/check-rust-module-deps.mjs` 的抽取器（去掉测试代码后重算，379 个文件 / 43 个模块 / 293 条边）分析 `packages/kimi-agent/src`：

- **一个 30 模块的强连通分量**（生产代码）：`acp, callbacks, compaction, config, default_workspace, environment, events, goal, i18n, injection, llm, mcp, native, permission, pipeline, project_local_config, prompt, region, rpc, server, session, skills, storage, subagent, swarm, tool_result_truncation, tools, turn_loop, workflow, workspace_trust`。
- DFS 回边 **98 条**。
- 真正的叶子（出度 0）：`primitives, tool_input_display, tool_result_truncation, turn_events, env, goal, knowledge, cron, frontmatter`。

结论：**搬文件不够**，必须先打断循环。循环的成因是「高层模块持有低层的具体类型」，解法是把类型下沉、把具体类型换成 trait。

## 关键的循环与破法

| 循环 | 具体边 | 破法 |
|---|---|---|
| `llm <-> turn_loop` | `llm/*` 用 `turn_loop::types::{ContentBlock, ToolCall, LLMMessage, ToolInfo, LLMChatResponse}`；`turn_loop` 用 `llm::LlmError` 等 | 把 `turn_loop/types.rs` 的 wire/LLM 类型下沉到 `kimi-core::types`；`llm` 与 `turn_loop` 都只依赖 core |
| `rpc <-> callbacks` | `rpc/types.rs` 用 `callbacks::MAIN_AGENT_ID`；`callbacks` 用 `rpc::types`、`rpc::server::RpcServer` | `MAIN_AGENT_ID` 下沉 core；`RpcServer` 换成 trait（`kimi-host`），`rpc` 只依赖 core |
| `events <-> callbacks` | `events/project.rs` 用 `callbacks::MAIN_AGENT_ID`；`callbacks` 用 `events::EventBus` | `MAIN_AGENT_ID` 与 `EventBus` 下沉 core |
| `tools <-> server` | `tools/file_ops.rs` 用 `server::files::FileStore` | `FileStore` 下沉 `kimi-storage`，`tools` 用 trait |
| `llm -> session` | `llm/media_resolver.rs` 用 `session::sqlite_store::SqliteSessionStore` 做上传缓存 | 定义 `UploadCache` trait（core/host），session 侧实现 |
| `llm -> injection` | `llm/wire.rs` 用 `injection::is_system_reminder` | 把该纯函数下沉 core |
| `llm -> callbacks` | `llm/proxy.rs`、`multi.rs` 用 `HostCallbacks` | `HostCallbacks` trait 下沉 `kimi-host` |
| `rpc -> permission/subagent/tools` | `rpc/types.rs` 的 `policy_snapshot`/`sandbox_mode`/`summary_policy` 字段 | 三个类型下沉 core |
| `callbacks <-> turn_loop` | `callbacks` 用 `turn_loop::types`；`turn_loop` 用 `callbacks` | 类型下沉 core 后，`turn_loop` 依赖 `kimi-host` |

## crate 边界（依赖顺序，自底向上）

| crate | 内容 | 依赖 |
|---|---|---|
| `kimi-core` | `primitives` `env` `turn_events` `frontmatter` `tool_input_display` `tool_result_truncation` `goal` `knowledge` `cron` + wire/LLM 类型 + `MAIN_AGENT_ID` `EventBus` `is_system_reminder` + `PolicySnapshot` `SandboxMode` `SummaryPolicy` | 无 |
| `kimi-host` | `HostCallbacks` trait、`RpcServer` trait、callback 相关类型 | core |
| `kimi-rpc` | `rpc`（types + server） | core, host |
| `kimi-permission` | `permission` | core |
| `kimi-storage` | `storage` `session` + `FileStore` + `UploadCache` 实现 | core, host |
| `kimi-llm` | `llm` | core, host, rpc |
| `kimi-tools` | `tools` | core, host, llm, permission, storage |
| `kimi-turn-loop` | `turn_loop` | core, host, llm, tools, permission, storage |
| `kimi-server` | `server` `acp` `repl` `napi_bindings` `config` `pipeline` `injection` `prompt` `compaction` `subagent` `swarm` `mcp` `skills` `callbacks`(impl) `events`(impl) `region` `default_workspace` `project_local_config` `workspace_trust*` `workflow` `environment` `i18n` `native` | 以上全部 |

`kimi-agent` 保留为 napi/cdylib 的薄外壳（`lib.rs` re-export + `main.rs`），保持 `kimi_agent::…` 对外路径不变。

## 阶段

每个阶段独立可交付、可回滚，且必须让门禁保持绿。

- **P0 `kimi-core`**：下沉类型与叶子。破 `llm<->turn_loop`、`rpc<->callbacks`、`events<->callbacks`。
- **P1 `kimi-host`**：抽出 `HostCallbacks`/`RpcServer` trait。
- **P2 `kimi-rpc`** + **`kimi-permission`**（两者都只依赖 core/host，可并行）。
- **P3 `kimi-storage`**：`storage` + `session` + `FileStore`，引入 `UploadCache`。
- **P4 `kimi-llm`**：依赖 core/host/rpc。
- **P5 `kimi-tools`**：引入 `FileStore` trait。
- **P6 `kimi-turn-loop`**。
- **P7 `kimi-server`**：其余全部。

## 备选方案与拒绝理由

- **A. 只加 `[profile.dev] debug = 0`**：实测热重编 43.8s → 21.7s（约 2x），一行改动、零风险。**但不改结构**，改动仍要重编整个 crate。可作为拆分的过渡期止损，不能替代拆分。
- **B. 只搬文件不打破循环**：不可行。依赖图是一个 SCC，Cargo 无法处理 crate 间循环依赖。
- **C. 用 `codegen-units` / `sccache` 提速**：`codegen-units` 影响并行度不影响重编范围；`sccache` 对本 crate 的「整 crate 重编」无效（指纹变了整 crate 就是一个新编译任务）。
- **D. 保留单 crate，改用 `cargo check` 迭代**：已采纳为日常习惯（check 不 codegen），但单次成本不变，且不解决「任何改动都全量」的根本问题。

## 与并行会话的协调

工作树内有另一个会话的 332 个未提交文件（`llm/`、`turn_loop/`、`server/` 等）。拆分第一步（P0）会改动 `llm/`、`turn_loop/`、`rpc/`、`callbacks` 的 import 路径，**与它正面冲突**。

因此 P0 之前需要二选一（在 `ExitSpecMode` 里作为选项提交）：
- 等并行会话收口（提交或明确交接）再开工；
- 在独立 worktree 上开工，基于 `main` HEAD，产出分支后择机合并。
