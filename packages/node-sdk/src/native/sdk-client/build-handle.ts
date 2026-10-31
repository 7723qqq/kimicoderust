import { randomUUID } from 'node:crypto';

import { EngineSessionHandle, type SessionCallbacks } from '@moonshot-ai/kimi-agent/session-handle';
import { createKimiDefaultHeaders, parseKimiCodeCustomHeaders } from '@moonshot-ai/kimi-code-oauth';
import type { CronJobOrigin, TaskInfo } from '@moonshot-ai/protocol';

import { type AgentFileDefinition } from '#/agent-file';
import type { KimiAuthFacade } from '#/auth';
import { loadRuntimeConfigLenient } from '#/config-local';
import { ErrorCodes, KimiError } from '#/error-protocol';
import type { QuestionItem, ToolInputDisplay } from '#/events';
import { type SetSessionPlanModeRpcInput } from '#/rpc';
import type { KimiHostIdentity, PromptOrigin, TelemetryClient } from '#/types';

import {
  resolveNativeLlm,
  resolveNativeLlmForAlias,
  buildPolicySnapshot,
  resolveSecondaryModelPool,
  resolveMultiLlmProviders,
  resolveGithubCredentials,
  resolveSubagentTimeoutMs,
  resolveSwarmTimeoutMs,
  resolveMaxAttemptsPerStep,
  resolveCompactionMaxAttempts,
  resolveMaxStepsPerTurn,
  resolveWebSearchService,
  resolveWebFetchService,
  resolveBingApiService,
  resolveImageReadByteBudget,
  resolveImageMaxEdgePx,
  resolveModelCapabilities,
  resolveModelContextWindow,
  resolveBackgroundLimits,
  resolvePrintBackground,
  providerTypeForAlias,
} from '../native-llm-resolver';
import type { SDKRpcClientNative } from '../sdk-rpc-client-native';
import { isApprovedForSession, rememberSessionApproval } from '../session-approvals';
import {
  toTokenUsage,
  normalizeEngineTurnId,
  toTurnEndReason,
  toKimiErrorCode,
  applySessionLlmOverrides,
  isExperimentalFlagEnabled,
  engineSwarmTrigger,
  resolveMcpServersForEngine,
} from './helpers';
import type { StoredMcpServerConfig, NativeSessionMeta } from './helpers';

/** The client surface the engine handle assembly needs, passed explicitly. */
export interface BuildHandleDeps {
  homeDir: string;
  configPath: string;
  /** The shell the Bash tool will run, resolved once per client (see `resolvedShellPath`). */
  shellPath: string | undefined;
  identity: KimiHostIdentity | undefined;
  telemetry: TelemetryClient;
  auth: KimiAuthFacade;
  liveSessions: Map<string, NativeSessionMeta>;
  receiveEvent: SDKRpcClientNative['receiveEvent'];
  requestApproval: SDKRpcClientNative['requestApproval'];
  requestQuestion: SDKRpcClientNative['requestQuestion'];
  persistMeta: (meta: NativeSessionMeta) => void;
  requireSession: (sessionId: string) => NativeSessionMeta;
  authorizedRoots: (meta: NativeSessionMeta) => string[];
  refreshProjectRoots: (meta: NativeSessionMeta) => Promise<void>;
  resolveSessionAgentProfiles: (meta: NativeSessionMeta) => Promise<readonly AgentFileDefinition[]>;
  setPlanMode: (input: SetSessionPlanModeRpcInput) => Promise<void>;
  loadGlobalMcpConfig: () => Record<string, StoredMcpServerConfig>;
}

export async function buildSessionHandle(
  deps: BuildHandleDeps,
  meta: NativeSessionMeta,
): Promise<EngineSessionHandle> {
  const sessionId = meta.id;
  const workDir = meta.workDir;
  // Re-read the project's roots on every build: the sandbox is constructed
  // from them here, so trusting or revoking the workspace takes effect on the
  // next handle build rather than needing a new session.
  await deps.refreshProjectRoots(meta);
  const authorizedRoots = deps.authorizedRoots(meta);
  const config = loadRuntimeConfigLenient(deps.configPath);
  const shellPath = deps.shellPath;
  const defaultHeaders = {
    ...parseKimiCodeCustomHeaders(),
    ...(deps.identity ? createKimiDefaultHeaders({ homeDir: deps.homeDir, ...deps.identity }) : {}),
  };
  const modelAlias = meta.model ?? config.defaultModel;
  const resolvedLlm = modelAlias
    ? resolveNativeLlmForAlias(config, modelAlias, undefined, defaultHeaders)
    : resolveNativeLlm(config, defaultHeaders);
  const nativeLlm = resolvedLlm ? applySessionLlmOverrides(resolvedLlm, meta) : resolvedLlm;
  // `[agent].multi_llm`: the concurrent-provider race. A non-empty list
  // outranks `nativeLlm` in the engine's LLM selection, so a configured race
  // is what the session runs. Throws `config.invalid` on an alias that cannot
  // resolve rather than silently racing fewer providers.
  const multiLlmProviders = resolveMultiLlmProviders(config, defaultHeaders);

  // 1-based LLM step counter for the current turn, reset by `turn.started`
  // in `turnEvent` below. The turn loop emits exactly one `llm.step.begin`
  // and one `llm.step.end` per step, both naming the turn and the step; the
  // host still counts on its own so the numbering stays contiguous if an
  // engine-side boundary is ever dropped.
  let stepSeq = 0;

  // Background tasks seen this handle. `event.task.completed` carries only
  // the id and status, so the create-time facts are remembered to fill the
  // terminal `background.task.terminated` info.
  const backgroundTasks = new Map<
    string,
    { description: string; kind: 'agent' | 'process'; startedAt: number; subagentType?: string }
  >();

  const callbacks: SessionCallbacks = {
    // Fail loud, never fake a reply. When no provider is configured the Rust
    // pipeline falls back to the host `llm_chat` proxy; throwing here surfaces
    // the missing provider config on the first turn instead of returning a
    // canned "Hello! I am Kimi Code." that ends the turn looking like a real
    // answer. createSession still succeeds (the proxy pipeline builds), so the
    // TUI can start and route the user to /login rather than failing to boot.
    llmChat: async () => {
      throw new KimiError(
        ErrorCodes.NOT_IMPLEMENTED,
        'native harness has no host LLM proxy — configure [providers.*] or [agent] nativeLlmProvider so the Rust engine calls the model directly',
      );
    },
    executeTool: async (req: string) => {
      // The Rust engine routes only host-owned tools here (MCP tools,
      // select_tools — everything not in NATIVE_TOOL_NAMES). The native
      // harness has no host tool registry yet, so fail loud *naming the tool*.
      // The previous path delegated to the base `toolCall`, which returned a
      // generic "SDK custom tool calls are not supported: <id>" that dropped
      // the tool name and read to the model like a transient failure to retry.
      //
      // Wire shape: the return value is a serde-deserialized
      // `ToolExecuteResponse` (`kimi-agent/src/rpc/types.rs`), whose required
      // fields are `content` + `is_error` — NOT the `{output, isError}` pair
      // the rest of the SDK uses. Emitting the wrong keys made every host
      // fallback die as `execute_tool parse: missing field \`content\``,
      // replacing the intended message with an opaque serde error.
      let parsed: { tool_call_id?: string; tool_name?: string; arguments?: unknown };
      try {
        parsed = JSON.parse(req);
      } catch {
        return JSON.stringify({ content: 'malformed tool execute request', is_error: true });
      }
      const toolName = parsed.tool_name ?? 'unknown';
      return JSON.stringify({
        content: `tool "${toolName}" is host-owned and not yet wired on the native harness`,
        is_error: true,
      });
    },
    emitEvent: (eventJson: string) => {
      // Parse defensively, then forward only events we can map onto a typed
      // protocol Event. Unknown snake_case engine events are dropped rather
      // than spread into the stream (the old `...parsed` fallback produced
      // payloads consumers could not discriminate against the union).
      let parsed;
      try {
        parsed = JSON.parse(eventJson);
      } catch {
        return;
      }
      // Agent attribution, most reliable source first.
      //
      // The engine stamps `agent_id` on every event a subagent's turn emits
      // (see `ToolFilterCallbacks::emit_event` in
      // `kimi-agent/src/subagent/manager.rs`); the main agent's events carry
      // `"main"`. That stamp is authoritative.
      //
      // The `subturn-` prefix test below is a legacy fallback for events
      // that predate the stamp. It cannot stand in for it: it only ever
      // matches the turn-level `llm.delta` stream, and the subagent id it
      // resolves to is `meta.activeAgentId` — the *side channel's* currently
      // active agent, which is not the agent that emitted the event. With
      // `AgentSwarm` running several subagents at once they all share it, so
      // every subagent event was attributed to the same id; and tool events
      // (`tool.native`) do not carry a `subturn-` turn id at all, so they
      // fell through to `'main'` outright — a subagent's tool call was
      // announced as the main agent's and landed in the main transcript,
      // truncating the main agent's own reasoning. Prefer the stamp.
      const eventAgentId =
        typeof parsed.agent_id === 'string'
          ? parsed.agent_id
          : typeof parsed.turn_id === 'string' && parsed.turn_id.startsWith('subturn-')
            ? (meta.activeAgentId ?? 'main')
            : 'main';
      if (process.env['KIMI_DEBUG_AGENT_ID'] === '1') {
        console.error(
          `[agent-id] type=${String(parsed.type)} raw_agent_id=${JSON.stringify(parsed.agent_id)} turn_id=${JSON.stringify(parsed.turn_id)} -> ${eventAgentId}`,
        );
      }
      if (parsed.type === 'llm.delta') {
        if (parsed.part?.type === 'text' && typeof parsed.part.text === 'string') {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'assistant.delta',
            turnId: meta.currentTurnId,
            delta: parsed.part.text,
          });
        } else if (
          (parsed.part?.type === 'think' || parsed.part?.type === 'thinking') &&
          typeof (parsed.part.think ?? parsed.part.thinking ?? parsed.part.text) === 'string'
        ) {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'thinking.delta',
            turnId: meta.currentTurnId,
            delta: String(parsed.part.think ?? parsed.part.thinking ?? parsed.part.text),
          });
        } else if (parsed.part?.type === 'tool_call' && typeof parsed.part.id === 'string') {
          // Tool-call argument fragments stream as `tool_call` parts
          // (`llm/wire.rs::StreamDelta::to_part`); without this arm the TUI
          // never saw the arguments being built.
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'tool.call.delta',
            turnId: meta.currentTurnId,
            toolCallId: parsed.part.id,
            ...(typeof parsed.part.arguments === 'string'
              ? { argumentsPart: parsed.part.arguments }
              : {}),
          });
        }
      } else if (parsed.type === 'tool.native') {
        // One id for the started/result pair (M3): two independent
        // randomUUID() calls orphaned the result whenever the engine sent no
        // tool_call_id, leaving the TUI tool card stuck in "running".
        const toolCallId = String(parsed.tool_call_id ?? randomUUID());
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'tool.call.started',
          turnId: meta.currentTurnId,
          toolCallId,
          name: String(parsed.tool_name ?? 'tool'),
          args: parsed.arguments ?? {},
        });
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'tool.result',
          turnId: meta.currentTurnId,
          toolCallId,
          output: String(parsed.content ?? ''),
          isError: Boolean(parsed.is_error),
        });
      } else if (parsed.type === 'tool.native.progress') {
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'tool.progress',
          turnId: meta.currentTurnId,
          // Progress must carry the same id as its started event to route to
          // the right card; a fresh uuid here would never match, so fall back
          // to empty (unrouted) rather than a bogus id.
          toolCallId: String(parsed.tool_call_id ?? ''),
          update: { kind: 'stdout', text: String(parsed.text ?? '') },
        });
      } else if (parsed.type === 'llm.step.begin') {
        // Turn-loop step boundary (`run_turn`). The transport used to emit one
        // per chat *request*, which also counted compaction summarizer calls
        // and retry attempts — a `turn.step.started` with nothing to close it
        // and an ordinal that skipped ahead. Only the loop can name a step,
        // so only the loop's boundary reaches the host now. The protocol step
        // events drive the TUI's step counter and streaming phase; without
        // this arm the step display stayed at 0 for the whole turn.
        stepSeq += 1;
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'turn.step.started',
          turnId: meta.currentTurnId,
          step: stepSeq,
        });
      } else if (parsed.type === 'llm.step.end') {
        // Two emitters send `llm.step.end`. The transport (llm/http.rs)
        // hands the host the assistant text plus `latency_ms` and names no
        // turn or step — it is what folds the message. The turn loop's
        // emission is the one that addresses a step, and carries the same
        // usage plus the v2 `ModelRequestTiming` breakdown that
        // `turn.step.completed` is specified to carry. Mapping both gave
        // every step a second `turn.step.completed` with the same turnId,
        // step and usage, so the TUI counted the same token usage twice
        // and a `filtered` finish was announced twice. Only the addressed
        // one maps; the transport's stays the engine's own message fold.
        if (parsed.turn_id !== undefined || parsed.step !== undefined) {
          const usage = toTokenUsage(parsed.usage);
          const timing = parsed.timing as
            | {
                firstTokenLatencyMs?: number;
                requestBuildMs?: number;
                serverFirstTokenMs?: number;
                streamDurationMs?: number;
              }
            | null
            | undefined;
          const numberFrom = (value: unknown): number | undefined =>
            typeof value === 'number' ? value : undefined;
          const firstTokenLatencyMs = numberFrom(timing?.firstTokenLatencyMs);
          const streamDurationMs = numberFrom(timing?.streamDurationMs);
          const requestBuildMs = numberFrom(timing?.requestBuildMs);
          const serverFirstTokenMs = numberFrom(timing?.serverFirstTokenMs);
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'turn.step.completed',
            turnId: meta.currentTurnId,
            step: stepSeq,
            ...(usage !== undefined ? { usage } : {}),
            ...(typeof parsed.finish_reason === 'string'
              ? { finishReason: parsed.finish_reason }
              : {}),
            ...(firstTokenLatencyMs !== undefined
              ? { llmFirstTokenLatencyMs: firstTokenLatencyMs }
              : {}),
            ...(streamDurationMs !== undefined ? { llmStreamDurationMs: streamDurationMs } : {}),
            ...(requestBuildMs !== undefined ? { llmRequestBuildMs: requestBuildMs } : {}),
            ...(serverFirstTokenMs !== undefined
              ? { llmServerFirstTokenMs: serverFirstTokenMs }
              : {}),
          });
        }
      } else if (parsed.type === 'subagent.spawned') {
        if (typeof parsed.subagent_id === 'string') {
          meta.agents = meta.agents ?? {
            main: { homedir: meta.workDir, type: 'main', parentAgentId: null },
          };
          meta.agents[parsed.subagent_id] = {
            homedir: meta.workDir,
            type: 'sub',
            parentAgentId: 'main',
          };
          deps.persistMeta(meta);
          // Forward the spawn itself too: the TUI creates the subagent card
          // from it, and the meta write above is host bookkeeping only.
          // `swarm_index` is what distinguishes a swarm member from a plain
          // child (v2 `coreEventMap.ts:1110-1117` reads it to pick the
          // `member` role), and the engine emits it — dropping it here is
          // what made every swarm member render as an ordinary subagent.
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'subagent.spawned',
            subagentId: parsed.subagent_id,
            subagentName: String(parsed.subagent_name ?? ''),
            parentToolCallId: String(parsed.parent_tool_call_id ?? ''),
            runInBackground: parsed.run_in_background === true,
            ...(typeof parsed.swarm_index === 'number' ? { swarmIndex: parsed.swarm_index } : {}),
            ...(typeof parsed.description === 'string' ? { description: parsed.description } : {}),
          });
        }
      } else if (parsed.type === 'subagent.started') {
        if (typeof parsed.subagent_id === 'string') {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'subagent.started',
            subagentId: parsed.subagent_id,
          });
        }
      } else if (parsed.type === 'subagent.suspended') {
        // A swarm member the engine requeued after a provider rate limit. It
        // is neither running nor failed, so without this arm the TUI's
        // swarm progress row keeps showing it as running until the retry
        // lands (or never).
        if (typeof parsed.subagent_id === 'string') {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'subagent.suspended',
            subagentId: parsed.subagent_id,
            reason: String(parsed.reason ?? ''),
            ...(typeof parsed.parent_tool_call_id === 'string'
              ? { parentToolCallId: parsed.parent_tool_call_id }
              : {}),
          });
        }
      } else if (parsed.type === 'swarm_mode.enter' || parsed.type === 'swarm_mode.exit') {
        // The engine owns the mode and announces both edges. Fold it into
        // `meta` so the next handle rebuild re-sends the mode the engine
        // actually has: an auto-exited `task` swarm must not be resurrected by
        // a stale `swarmMode: true`.
        const active = parsed.type === 'swarm_mode.enter';
        if (meta.swarmMode !== active) {
          meta.swarmMode = active;
          deps.persistMeta(meta);
        }
      } else if (parsed.type === 'agent.status.updated') {
        // The engine's edge-triggered mode notifications (`swarmMode`
        // without the other status fields). Fold the mode and forward the
        // event so the TUI can clear its indicator and render the "ended"
        // marker for a `task` swarm; a mode the host only tracks itself
        // never reports its own end.
        if (typeof parsed.swarmMode === 'boolean' && meta.swarmMode !== parsed.swarmMode) {
          meta.swarmMode = parsed.swarmMode;
          if (!parsed.swarmMode) meta.swarmModeTrigger = 'manual';
          deps.persistMeta(meta);
        }
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'agent.status.updated',
          ...(typeof parsed.swarmMode === 'boolean' ? { swarmMode: parsed.swarmMode } : {}),
        });
      } else if (parsed.type === 'subagent.completed') {
        if (typeof parsed.subagent_id === 'string') {
          const usage = toTokenUsage(parsed.usage);
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'subagent.completed',
            subagentId: parsed.subagent_id,
            resultSummary: String(parsed.result_summary ?? ''),
            ...(usage !== undefined ? { usage } : {}),
          });
        }
      } else if (parsed.type === 'subagent.failed') {
        if (typeof parsed.subagent_id === 'string') {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'subagent.failed',
            subagentId: parsed.subagent_id,
            error: String(parsed.error ?? ''),
          });
        }
      } else if (parsed.type === 'subagent.cancelled') {
        if (typeof parsed.subagent_id === 'string') {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'subagent.cancelled',
            subagentId: parsed.subagent_id,
          });
        }
      } else if (parsed.type === 'event.task.created') {
        // The engine's per-pipeline task runner reports its lifecycle here
        // (the napi pipeline wires its sink to the host callbacks). The TUI
        // consumes the protocol `background.task.*` spelling, so map it: the
        // engine's `subagent` kind is the protocol's `agent`, everything else
        // (bash / tool) is a `process`.
        const task = parsed.task as Record<string, unknown> | undefined;
        if (task !== undefined && typeof task['id'] === 'string') {
          const kind = task['kind'] === 'subagent' ? 'agent' : 'process';
          const startedAt =
            (typeof task['started_at'] === 'string'
              ? Date.parse(task['started_at'])
              : Number.NaN) || Date.now();
          const description = typeof task['description'] === 'string' ? task['description'] : '';
          const subagentType =
            typeof task['subagent_type'] === 'string' ? task['subagent_type'] : undefined;
          backgroundTasks.set(task['id'], {
            description,
            kind,
            startedAt,
            ...(subagentType !== undefined ? { subagentType } : {}),
          });
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'background.task.started',
            // The Rust runner does not own the process, so it carries no
            // `command` / `pid`; no consumer of this event reads them (the
            // task browser pulls `listBackgroundTasks` for the full row).
            info: {
              taskId: task['id'],
              description,
              status: 'running',
              kind,
              startedAt,
              endedAt: null,
              detached: true,
              ...(subagentType !== undefined ? { subagentType } : {}),
              // Agent tasks use the agent id as their task id
              // (`subagent/manager.rs` spawns with `id`), so this is the id
              // the TUI's detach path matches on.
              ...(kind === 'agent' ? { agentId: task['id'] } : {}),
            } as unknown as TaskInfo,
          });
        }
      } else if (parsed.type === 'event.task.completed') {
        const taskId = typeof parsed.task_id === 'string' ? parsed.task_id : undefined;
        if (taskId !== undefined) {
          const prior = backgroundTasks.get(taskId);
          backgroundTasks.delete(taskId);
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'background.task.terminated',
            info: {
              taskId,
              description: prior?.description ?? '',
              status: parsed.status as TaskInfo['status'],
              kind: prior?.kind ?? 'process',
              startedAt: prior?.startedAt ?? Date.now(),
              endedAt: Date.now(),
              detached: true,
              ...(prior?.subagentType !== undefined ? { subagentType: prior.subagentType } : {}),
              ...(prior?.kind === 'agent' ? { agentId: taskId } : {}),
            } as unknown as TaskInfo,
          });
        }
      } else if (parsed.type === 'compaction.started') {
        // The turn loop's automatic compaction (v2 `compaction.started`).
        // The host adds the session id; the manual `/compact` path emits its
        // own events directly, so there is no double emit.
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'compaction.started',
          trigger: parsed.trigger === 'manual' ? 'manual' : 'auto',
          ...(typeof parsed.instruction === 'string' ? { instruction: parsed.instruction } : {}),
        });
      } else if (parsed.type === 'compaction.completed') {
        const result = parsed.result as Record<string, unknown> | undefined;
        if (result !== undefined) {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'compaction.completed',
            result: {
              summary: typeof result['summary'] === 'string' ? result['summary'] : '',
              compactedCount: Number(result['compactedCount'] ?? 0),
              tokensBefore: Number(result['tokensBefore'] ?? 0),
              tokensAfter: Number(result['tokensAfter'] ?? 0),
            },
          });
        }
      } else if (parsed.type === 'compaction.cancelled') {
        deps.receiveEvent({ sessionId, agentId: eventAgentId, type: 'compaction.cancelled' });
      } else if (parsed.type === 'hook.result') {
        // The hook guard's stdout + verdict, surfaced so the transcript can
        // show what the user's hook said (v2 `hook.result`).
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'hook.result',
          turnId: meta.currentTurnId,
          hookEvent: String(parsed.hookEvent ?? ''),
          content: String(parsed.content ?? ''),
          ...(parsed.blocked === true ? { blocked: true } : {}),
        });
      } else if (parsed.type === 'cron.fired') {
        // The session-owned cron dispatcher: a due job publishes this (for
        // the TUI's cron card) and enqueues its own `<cron-fire>` turn.
        const origin = parsed.origin as Record<string, unknown> | undefined;
        if (origin !== undefined) {
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'cron.fired',
            origin: origin as unknown as CronJobOrigin,
            prompt: String(parsed.prompt ?? ''),
          });
        }
      } else if (parsed.type === 'mcp.server.status') {
        // The MCP manager starts its connects in the background, so a server
        // that is still `pending` when the session is created only ever
        // reaches the UI through this transition (v2 `McpServerStatus`,
        // agent-core-v2 `agent/mcp/mcpService.ts:168-180`). The TUI resolves
        // its startup status spinner from it.
        const server = parsed.server as Record<string, unknown> | undefined;
        if (server !== undefined && typeof server['name'] === 'string') {
          const rawTransport = server['transport'];
          const transport =
            rawTransport === 'http' || rawTransport === 'sse' ? rawTransport : 'stdio';
          deps.receiveEvent({
            sessionId,
            agentId: eventAgentId,
            type: 'mcp.server.status',
            server: {
              name: server['name'],
              transport,
              status: server['status'] as
                | 'pending'
                | 'connected'
                | 'failed'
                | 'disabled'
                | 'needs-auth'
                | 'removed',
              toolCount: typeof server['toolCount'] === 'number' ? server['toolCount'] : 0,
              ...(typeof server['error'] === 'string' ? { error: server['error'] } : {}),
            },
          });
        }
      } else if (parsed.type === 'skill.activated') {
        // The engine's native Skill tool records model-tool activations (v2
        // `SkillTool.execution`): forward the same fields the host-side
        // user-slash activation sends.
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'skill.activated',
          activationId: String(parsed.activationId ?? ''),
          skillName: String(parsed.skillName ?? ''),
          ...(typeof parsed.skillArgs === 'string' && parsed.skillArgs.length > 0
            ? { skillArgs: parsed.skillArgs }
            : {}),
          trigger: parsed.trigger === 'nested-skill' ? 'nested-skill' : 'model-tool',
          ...(typeof parsed.skillPath === 'string' ? { skillPath: parsed.skillPath } : {}),
          ...(typeof parsed.skillSource === 'string' ? { skillSource: parsed.skillSource } : {}),
        });
      } else if (parsed.type === 'warning') {
        // Engine-side turn warnings (media budget, MCP startup, …). Without
        // this arm they were dropped on the floor: the engine emitted them
        // and no consumer ever saw one.
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'warning',
          message: String(parsed.message ?? ''),
          ...(typeof parsed.code === 'string' ? { code: parsed.code } : {}),
        });
      } else if (parsed.type === 'error') {
        // A turn that failed outright (session/mod.rs emits this beside the
        // failed `turn.ended`, mirroring v2's AgentErrorEvent). The code is
        // engine-supplied and validated against the protocol union by the
        // consumer, so an unknown one degrades to `internal` rather than
        // dropping the message.
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'error',
          code: toKimiErrorCode(parsed.code),
          message: String(parsed.message ?? ''),
          retryable: parsed.retryable === true,
        });
      }
    },
    telemetry: (eventJson: string) => {
      // The engine's turn telemetry (M1c): `turn_started` / `turn_ended` /
      // `turn_interrupted`, the host-injected context below merged with the
      // engine-observed outcome (reason / duration_ms / steps / trace_id).
      // v2's loopService emits the same events through its telemetry
      // service; forwarding them under the engine's event name keeps that
      // contract. The payload's `event` field is the name, the rest are the
      // properties.
      let parsed: unknown;
      try {
        parsed = JSON.parse(eventJson);
      } catch {
        return;
      }
      if (typeof parsed !== 'object' || parsed === null) return;
      const { event: name, ...rest } = parsed as Record<string, unknown>;
      if (typeof name !== 'string' || name.length === 0) return;
      // Only primitives ride the wire (the telemetry client drops the rest
      // anyway); the engine's payload is all strings and numbers.
      const properties: Record<string, boolean | number | string | undefined | null> = {};
      for (const [key, value] of Object.entries(rest)) {
        if (
          value === null ||
          typeof value === 'boolean' ||
          typeof value === 'number' ||
          typeof value === 'string'
        ) {
          properties[key] = value;
        }
      }
      deps.telemetry.track(name, properties);
    },
    checkPermission: async (req: string) => {
      try {
        const parsed = JSON.parse(req) as {
          tool_call_id?: string;
          tool_name?: string;
          action?: string;
          display?: ToolInputDisplay;
          reason?: string;
          /** The user's own ask rule that fired, when one did. */
          session_approval_rule?: string;
        };
        // The engine names the ask rule that fired, when one did. If the user
        // already approved that rule for this session, answering again would
        // re-ask a question they have answered — the whole point of the
        // "for this session" choice.
        const approvalRule =
          typeof parsed.session_approval_rule === 'string' &&
          parsed.session_approval_rule.length > 0
            ? parsed.session_approval_rule
            : undefined;
        const sessionMeta = deps.liveSessions.get(sessionId);
        if (isApprovedForSession(approvalRule, sessionMeta?.sessionApprovals ?? [])) {
          return JSON.stringify({ decision: 'allow' });
        }
        const res = await deps.requestApproval({
          sessionId,
          agentId: 'main',
          toolCallId: parsed.tool_call_id ?? randomUUID(),
          action: parsed.action ?? 'execute',
          toolName: parsed.tool_name ?? 'unknown',
          display: parsed.display ?? { kind: 'command', command: parsed.tool_name ?? 'action' },
          // The engine's policy-specific explanation, already localized. Passed
          // through so the prompt can say why it is asking rather than only
          // naming the tool; omitted when the engine has nothing to add.
          ...(typeof parsed.reason === 'string' && parsed.reason.length > 0
            ? { reason: parsed.reason }
            : {}),
        });
        if (res.decision === 'approved') {
          // The promotion rule lives in `session-approvals.ts` so it is tested
          // directly: an inline copy would be covered by nothing, and the two
          // directions that matter (a rejection must never be remembered, a
          // different rule must still ask) are exactly what a plumbing test
          // would miss.
          if (sessionMeta !== undefined) {
            const remembered = rememberSessionApproval(
              approvalRule,
              { decision: res.decision, scope: res.scope },
              sessionMeta.sessionApprovals,
            );
            sessionMeta.sessionApprovals = [...remembered];
          }
          return JSON.stringify({ decision: 'allow' });
        }
        return JSON.stringify({ decision: 'deny', reason: res.feedback ?? 'User rejected' });
      } catch {
        return JSON.stringify({ decision: 'deny', reason: 'cancelled' });
      }
    },
    askQuestion: async (req: string) => {
      try {
        const parsed = JSON.parse(req) as { tool_call_id?: string; questions?: QuestionItem[] };
        const res = await deps.requestQuestion({
          sessionId,
          agentId: 'main',
          toolCallId: parsed.tool_call_id ?? randomUUID(),
          questions: parsed.questions ?? [],
        });
        if (res === null) {
          return JSON.stringify({ answers: {}, cancelled: true, reason: 'cancelled' });
        }
        const rawAnswers = 'answers' in res ? res.answers : res;
        const answers: Record<string, string> = {};
        for (const [k, v] of Object.entries(rawAnswers)) {
          answers[k] = typeof v === 'string' ? v : 'true';
        }
        const method = 'method' in res ? res.method : undefined;
        return JSON.stringify({ answers, ...(method ? { method } : {}) });
      } catch {
        return JSON.stringify({ answers: {}, cancelled: true, reason: 'error' });
      }
    },
    stateRead: async (req: string) => {
      // The engine's plan guard reads plan mode through this bridge before
      // every guarded native call (M5 made the `plan` domain host-first), so
      // serving it from live meta lets setPlanMode take effect without
      // rebuilding the handle. Every other domain is engine-owned (todo /
      // goal / cron / task / turn): error so the Rust StateStoreCallbacks
      // falls back to its local store.
      //
      // The plan id and path ride along: `EnterPlanMode` renders its
      // workflow message from them, and without them the model is told to
      // wait for a plan file the host has already prepared.
      const parsed = JSON.parse(req) as { domain?: string };
      if (parsed.domain === 'plan') {
        return JSON.stringify({
          value: { active: meta.planMode, id: meta.plan?.id, path: meta.plan?.path },
        });
      }
      throw new Error('host does not support state bridge');
    },
    stateWrite: async (req: string) => {
      // The plan domain is host-owned, so the engine's EnterPlanMode /
      // ExitPlanMode write through this bridge and the host is what owns the
      // plan document and the `agent.status.updated` the UI switches on.
      // Without this arm the write fell through to the engine's local store
      // while the read came from here: EnterPlanMode reported success (and
      // wrote a plan file under the engine's own state dir), then
      // ExitPlanMode read `active: false` and refused to run.
      const parsed = JSON.parse(req) as { domain?: string; value?: { active?: boolean } };
      if (parsed.domain !== 'plan') {
        throw new Error('host does not support state bridge');
      }
      await deps.setPlanMode({ sessionId, enabled: parsed.value?.active === true });
      const current = deps.requireSession(sessionId);
      return JSON.stringify({
        ok: true,
        value: {
          active: current.planMode,
          id: current.plan?.id,
          path: current.plan?.path,
        },
      });
    },
    goal: async () => {
      // Fresh goal snapshot per turn (snake_case wire). Only an active goal
      // drives the engine's autonomous continuation; paused / terminal goals
      // report null so the turn loop does not keep going.
      const goal = meta.goal;
      if (!goal || goal.status !== 'active') return null;
      return JSON.stringify({
        goal_id: goal.goalId,
        objective: goal.objective,
        status: goal.status,
        wall_clock_ms: Date.now() - goal.createdAt,
        tokens_used: goal.tokensUsed,
        turns_used: goal.turnsUsed,
      });
    },
    turnEvent: (eventJson: string) => {
      let parsed;
      try {
        parsed = JSON.parse(eventJson);
      } catch {
        return;
      }
      const eventAgentId = meta.activeAgentId ?? 'main';
      const rawTurnId = parsed.turnId ?? parsed.turn_id;
      const turnId = normalizeEngineTurnId(rawTurnId);
      if (parsed.type === 'turn.started') {
        meta.currentTurnId = turnId;
        // New turn: restart the synthesized LLM step counter.
        stepSeq = 0;
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'turn.started',
          turnId,
          // The engine echoes the turn request's origin (v2 `PromptOrigin`
          // JSON): a user-slash skill activation carries the
          // `skill_activation` variant (v2 #3832), everything else `user`.
          // Forwarding it — rather than assuming `user` — is what lets an
          // origin-aware consumer (the survey gate, the transcript fold)
          // tell an activation turn from a typed one.
          origin: (parsed.origin as PromptOrigin | undefined) ?? { kind: 'user' },
          prompt: String(parsed.prompt ?? ''),
        });
      } else if (parsed.type === 'turn.ended') {
        const endedTurnId =
          parsed.turnId !== undefined || parsed.turn_id !== undefined ? turnId : meta.currentTurnId;
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'turn.ended',
          turnId: endedTurnId,
          reason: toTurnEndReason(parsed.reason ?? parsed.status),
          ...(parsed.error ? { error: parsed.error } : {}),
          ...((parsed.durationMs ?? parsed.duration_ms)
            ? { durationMs: parsed.durationMs ?? parsed.duration_ms }
            : {}),
          // v2 #3907: the survey payload ties a rating to the exact turn via
          // this id (engine-minted `turn-<id>`).
          ...(parsed.trace_id ? { traceId: parsed.trace_id } : {}),
        });
        meta.activeAgentId = undefined;
      } else if (parsed.type === 'turn.cancel') {
        // v2 `TurnCancel`. A turn cancelled while still queued never starts,
        // so it emits **no** `turn.ended` — this is the only terminal signal
        // a client ever gets for it. Dropping it here (the old behaviour)
        // left the busy indicator and the composer stuck forever.
        const target =
          parsed.target === 'active' || parsed.target === 'queued'
            ? (parsed.target as 'active' | 'queued')
            : undefined;
        const reason =
          parsed.reason === 'user_cancelled' || parsed.reason === 'aborted'
            ? (parsed.reason as 'user_cancelled' | 'aborted')
            : undefined;
        deps.receiveEvent({
          sessionId,
          agentId: eventAgentId,
          type: 'turn.cancel',
          ...(parsed.turnId !== undefined || parsed.turn_id !== undefined ? { turnId } : {}),
          ...(target !== undefined ? { target } : {}),
          ...(reason !== undefined ? { reason } : {}),
        });
      }
      // Unknown turn events are dropped (see emitEvent).
    },
  };

  // The `[secondary_model]` pool is always on (0.42.0 promoted the
  // experiment). It rides the session params as JSON; a malformed section
  // throws here, so the session fails at startup naming the offending alias.
  const secondaryModel = resolveSecondaryModelPool(config, true, process.env, defaultHeaders);
  const policySnapshot = buildPolicySnapshot(config, workDir);
  // The session's live permission mode overrides the config-derived snapshot
  // default: setPermission mutates meta, and a rebuild must reflect the
  // current mode. Plan mode is deliberately NOT folded in — it is not a
  // permission mode. v2's `PermissionMode` is `manual | yolo | auto`
  // (`agent/permissionPolicy/types.ts`) and `plan` is not even in the config
  // schema (`DefaultPermissionModeSchema`), because plan mode is a tool guard
  // of its own: `AgentPlanService.guardToolExecution` vetoes the writes that
  // are not the plan file. The engine runs that guard too, against the live
  // plan state (the plan-mode reminder says it outright: "Bash follows the
  // normal permission mode and rules"). Sending `plan` as a permission mode
  // made the engine degrade to its manual default, so a yolo session in plan
  // mode asked for every Bash command — a plan-mode toggle or any handle
  // rebuild (setThinking / setModel / additionalDirs) turned yolo off.
  policySnapshot.mode = meta.permissionMode;
  // Rules the user approved for this session. `buildPolicySnapshot` derives the
  // three decision lists from config and cannot see them: the approvals are a
  // record of decisions made in this session, not of what the config says.
  // Re-sending them on a rebuild is what keeps the engine's own
  // `SessionApprovalHistory` policy in step with the host's memory.
  policySnapshot.session_approvals = [...meta.sessionApprovals];
  // A headless session (upstream `nonInteractive`) drops the engine's
  // dangerous-command ask policy — there is no human to answer it.
  if (meta.nonInteractive) {
    policySnapshot.non_interactive = true;
  }
  const githubCreds = resolveGithubCredentials(config);
  const mcpConfig = deps.loadGlobalMcpConfig();
  const mcpServers = resolveMcpServersForEngine(mcpConfig);
  const subagentTimeoutMs = resolveSubagentTimeoutMs(config);
  const swarmTimeoutMs = resolveSwarmTimeoutMs(config);
  const maxAttempts = resolveMaxAttemptsPerStep(config);
  const compactionMaxAttempts = resolveCompactionMaxAttempts(config);
  const maxSteps = resolveMaxStepsPerTurn(config);
  const webSearch = resolveWebSearchService(config);
  const webFetch = resolveWebFetchService(config);
  const bingApi = resolveBingApiService(config);
  const imageReadByteBudget = resolveImageReadByteBudget(config);
  const imageMaxEdgePx = resolveImageMaxEdgePx(config);
  const modelCapabilities = resolveModelCapabilities(config, meta.model);
  // Progressive tool disclosure (v2 `toolSelectService.enabled()`): the
  // flag alone is not enough — the advertised table is only shaped when the
  // model also declares `dynamically_loaded_tools`, but the flag is what
  // the engine's gate reads.
  const toolSelect = isExperimentalFlagEnabled(config, 'tool_select');
  // The engine gates the advertised Tower* tool table on this and falls back
  // to its own bare env probe when it is absent — which never sees
  // `[experimental].tower`. Passing the host-resolved flag makes the config
  // key (and the master switch) reach the engine, same as tool_select.
  const towerEnabled = isExperimentalFlagEnabled(config, 'tower');
  const background = resolveBackgroundLimits(config);
  // Print mode (`kimi -p`): the host resolves only which `[background]`
  // values apply — the engine owns the settle behavior.
  const printBackground = resolvePrintBackground(config);
  // Agent-file discovery is host-side by design: the engine declares
  // `extra_agent_dirs` but never reads it (kimi-agent/src/config/mod.rs:588),
  // so which files exist and which scope wins is resolved here.
  const agentProfiles = await deps.resolveSessionAgentProfiles(meta);
  const authToken =
    nativeLlm?.authProvider === undefined
      ? undefined
      : (request: string) => {
          let parsed: { provider: string; force: boolean };
          try {
            parsed = JSON.parse(request) as { provider: string; force: boolean };
          } catch (error) {
            throw new KimiError(
              ErrorCodes.REQUEST_INVALID,
              `malformed auth token request from the engine: ${error instanceof Error ? error.message : String(error)}`,
            );
          }
          const tokenProvider = deps.auth.resolveOAuthTokenProvider(parsed.provider);
          return tokenProvider.getAccessToken({ force: parsed.force === true });
        };

  const params = {
    turnId: sessionId,
    sessionId,
    callerAgentId: 'main',
    rustSelfContained: config.agent?.rustSelfContained === true,
    // Empty means "let the engine build `prompt/system.md` for this
    // workspace" — AGENTS.md cascade, skills catalog, environment listing,
    // profile role. A host-owned prompt only ever arrives through
    // `native_llm.systemPrompt` (`[models.<alias>].systemPrompt`), which the
    // engine prefers anyway. The one-line stub that used to sit here reached
    // `messages[0]` verbatim and silently dropped all of it.
    systemPrompt: '',
    // The engine requires a modelName string even for a model-less session;
    // the SDK surface keeps `undefined` for the unbound state.
    modelName: nativeLlm?.model ?? meta.model ?? 'default',
    messages: [],
    tools: [],
    // The engine seeds its per-model buckets from the snapshot the host last
    // persisted; the host never re-derives them.
    initialUsageJson: meta.usageJson,
    workspaceRoot: workDir,
    nativeTools: config.agent?.nativeTools !== false,
    // `/add-dir` roots. The engine serves paths under them natively; without
    // this they fall outside `workspace_root` and the only fallback — the
    // host `execute_tool` seam — has no tool runtime to serve them.
    // `?? undefined` (never null): napi Option fields reject null.
    additionalDirs: authorizedRoots.length > 0 ? [...authorizedRoots] : undefined,
    shellPath,
    policySnapshotJson: JSON.stringify(policySnapshot),
    secondaryModelJson: secondaryModel === undefined ? undefined : JSON.stringify(secondaryModel),
    // Host-resolved config knobs (env > config, see native-llm-resolver).
    // `?? undefined` (never null): napi Option fields reject null.
    subagentTimeoutMs: subagentTimeoutMs ?? undefined,
    swarmTimeoutMs: swarmTimeoutMs ?? undefined,
    // The swarm mode the engine should be in for this handle's turns. The
    // engine owns the mode (its `swarm_mode` reminder and the turn-end
    // auto-exit read it), so this is the only way a `/swarm` toggle reaches
    // it over napi. `undefined` leaves it alone; `false` exits it.
    swarmMode: meta.swarmMode,
    swarmModeTrigger: meta.swarmMode ? engineSwarmTrigger(meta.swarmModeTrigger) : undefined,
    maxAttempts: maxAttempts ?? undefined,
    compactionMaxAttempts: compactionMaxAttempts ?? undefined,
    // The window `should_compact` compares history against, resolved for
    // *this session's* model — not `config.defaultModel`, which a session
    // switched with `/model` never matches. Without it the engine falls back
    // to its own 128k default and compacts a 1M-window model at ~81k tokens
    // (`128*1024 - 50_000` reserved). `[models.*].maxInputSize` still wins
    // inside `compaction_window`, matching v2's `max_input_tokens ??
    // max_context_tokens`. 0 (no declared window) means "engine default".
    maxContextTokens: resolveModelContextWindow(config, modelAlias) || undefined,
    maxSteps: maxSteps ?? undefined,
    webSearch: webSearch ?? undefined,
    webFetch: webFetch ?? undefined,
    bingApi: bingApi ?? undefined,
    imageReadByteBudget: imageReadByteBudget ?? undefined,
    imageMaxEdgePx: imageMaxEdgePx ?? undefined,
    modelCapabilities: modelCapabilities ?? undefined,
    toolSelect,
    towerEnabled,
    // Session profile catalog snapshot (P46): the native `Agent` tool spawns
    // only profiles registered here; without it every `Agent` call falls
    // back to the host, which has no tool runtime on this transport.
    subagentProfiles: agentProfiles.length > 0 ? [...agentProfiles] : undefined,
    // The concurrent-provider race (`[agent].multi_llm`). Non-empty outranks
    // `nativeLlm` in the engine's LLM selection.
    providers: multiLlmProviders ? [...multiLlmProviders] : undefined,
    // Main-agent profile (`--agent` / `--agent-file`): shapes the session's
    // own system prompt (`${role_additional}` + tool allowance), which is a
    // different consumer from `subagentProfiles` above.
    agentProfile: meta.agentProfile ?? undefined,
    // `[background]` knobs: the engine applies them to its own task runner
    // and Bash tool, so the file behaves the same on every entry point.
    killGracePeriodMs: background?.killGracePeriodMs,
    maxRunningTasks: background?.maxRunningTasks,
    bashAutoBackgroundOnTimeout: background?.bashAutoBackgroundOnTimeout,
    bashTaskTimeoutS: background?.bashTaskTimeoutS,
    // Print-mode settle policy (`kimi -p`): the engine holds the turn
    // receipt while the task runner drains.
    printBackgroundMode: printBackground.mode,
    printWaitCeilingS: printBackground.ceilingS,
    printMaxTurns: printBackground.maxTurns,
    ...(githubCreds.githubToken ? { githubToken: githubCreds.githubToken } : {}),
    ...(githubCreds.githubBaseUrl ? { githubBaseUrl: githubCreds.githubBaseUrl } : {}),
    ...(nativeLlm ? { nativeLlm } : {}),
    ...(mcpServers.length > 0 ? { mcpServers } : {}),
    // M1c turn telemetry: the host knows the model configuration, the
    // engine contributes the outcome fields and emits turn_started /
    // turn_ended / turn_interrupted through the telemetry callback (v2's
    // loopService owns the same events).
    // `enabled_plugins` is deliberately absent: the only source is the
    // engine's plugin registry, and reading it here would open that
    // SQLite store at every session creation (`ensurePluginStore`) —
    // a behavior change that also locks the data directory for the
    // session's lifetime. v2's semantics for a host without a plugin
    // snapshot is exactly this: the field stays absent. The toggle-time
    // set still rides the `plugin_toggle` event, which reads the
    // registry on demand.
    telemetry: {
      mode: meta.planMode ? 'plan' : 'agent',
      providerType: providerTypeForAlias(config, modelAlias),
      protocol: nativeLlm?.protocol ?? '',
      thinkingEffort: meta.thinkingEffort,
    },
  };

  return EngineSessionHandle.create(params, { ...callbacks, authToken });
}
