# 进度

## P0 kimi-core

- [x] 建 `packages/kimi-core`（Cargo.toml + lib.rs）。
- [x] 搬入 5 个真叶子：`primitives`、`env`、`turn_events`、`frontmatter`、`tool_input_display`。
- [x] 3 个 `pub(crate)` 项改 `pub`（`env::ENV_TEST_LOCK`、`turn_events` 两个函数）。
- [x] 下沉 wire 数据类型到 `kimi-core::types`（586 行）：`MediaKind`、`ContentBlock`、`NativeResponseFormat`、`NativeJsonSchema`、`NativeSampling`、`NativeDeterminism`、`NativeExtraParams`、`NativeLlmConfig`（含手工 `Debug`）、`AntigravityLimits`、`TokenUsage`、`ToolInfo`、`ToolDelivery`、`GoalStatus`、`GoalContext`、`TelemetryContext`。
- [x] `rpc/types.rs` 原位置 `pub use kimi_core::types::{…}`，1136 行。
- [x] `cargo check -p kimi-core --offline` 通过；`cargo check --lib --features cli --offline`（kimi-agent）通过。

## 卡在 `llm <-> turn_loop` 的剩余边

`turn_loop/types.rs` 仍引用 `llm`：

| 引用 | 性质 | 下沉难度 |
|---|---|---|
| `MediaTarget` | 纯数据 | 易 |
| `LlmTiming` | 纯数据 | 易 |
| `CredentialFuture` | 类型别名 | 易 |
| `DroppedMedia` / `SharedMediaRecovery` | 类型别名（std 上） | 易 |
| `ToolCallIdNormalizer` | 行为（739 行，无 crate 依赖） | 中 |
| `TurnWatchState` | 行为（无 crate 依赖） | 中 |
| `MediaResolver` | 行为（1087 行，依赖 `session::SqliteSessionStore`、`server::files::FileStore`、`callbacks`） | 难 |

`MediaResolver` 是硬点：`turn_loop` 在 `budgeted_request`（`run_turn.rs:575`）与 `apply_media_recovery`（`run_turn.rs:1282`）里**具体调用**它的 `resolve()` / `media_entries()`。要打断必须做依赖倒置：在 core 定义 `MediaResolverApi` trait，`RunTurnInput.media` 与 `budgeted_request` 改持 `&dyn`。这会改 `RunTurnInput` 与 `budgeted_request` 的公开签名，并把 `Entry`/`ResolvedRequest`/`UploadError` 一并下沉。

## 决策点

下沉前 6 项是机械工作；第 7 项是设计改动。等用户裁定是否做依赖倒置。
