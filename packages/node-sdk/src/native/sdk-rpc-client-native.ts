import { exec } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import {
  createWriteStream,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';

import type {
  EngineSessionHandle,
  SessionPrompt,
  SessionTurnOutcome,
} from '@moonshot-ai/kimi-agent/session-handle';
import type {
  CustomRegistryProviderEntry,
  CustomRegistrySource,
  ManagedKimiConfigShape,
} from '@moonshot-ai/kimi-code-oauth';
import {
  applyCustomRegistryEntries,
  assertKimiHostIdentity,
  createKimiDefaultHeaders,
  credentialEnvHints,
  CustomRegistryApiError,
  fetchCustomRegistry,
} from '@moonshot-ai/kimi-code-oauth';
import { estimateTokensForMessages } from '@moonshot-ai/kosong/tokens';
import { effectiveModelAlias } from '#/model-alias';
import type {
  GoalSnapshot as ProtocolGoalSnapshot,
  TurnEndReason,
} from '@moonshot-ai/protocol';
import { ZipFile } from 'yazl';

import {
  BUILTIN_AGENT_PROFILE_NAMES,
  discoverAgentFiles,
  type AgentFileDefinition,
  type DiscoveredAgentFiles,
} from '#/agent-file';
import { KimiAuthFacade } from '#/auth';
import { RegistryImportError } from '#/catalog';
import { ensureConfigFile as ensureConfigFileScaffold } from '#/config-helpers';
import {
  cloneRecord,
  loadRuntimeConfig,
  loadRuntimeConfigLenient,
  readConfigFile,
  validateConfig,
  writeConfigFile,
  type KimiConfig,
  type KimiConfigPatch,
} from '#/config-local';
import { resolveConfigPath, resolveKimiHome } from '#/config-local/path';
import { parseBooleanEnv } from '#/config-local/resolve';
import { ErrorCodes, KimiError } from '#/error-protocol';
import { ImageLimits } from '#/image-limits';
import { KimiHarness } from '#/kimi-harness';
import { flushDiagnosticLogs, getRootLogger, log, resolveLoggingConfig } from '#/logging';
import {
  appendAdditionalDir,
  readAdditionalDirs,
  resolveAdditionalDirs,
} from '#/project-local-config';
import {
  SDKRpcClientBase,
  type ActivatePluginCommandRpcInput,
  type ActivateSkillRpcInput,
  type ImportContextRpcInput,
  type ReloadSessionRpcInput,
  type SessionIdRpcInput,
  type SessionPromptRpcInput,
  type SessionPromptWithSkillsRpcInput,
  type SetSessionModelRpcInput,
  type SetSessionModelRpcResult,
  type SetSessionPermissionRpcInput,
  type SetSessionPlanModeRpcInput,
  type SetSessionSwarmModeRpcInput,
  type SetSessionThinkingRpcInput,
  type SetSessionTowerModeRpcInput,
  type UpdateSessionMetadataRpcInput,
} from '#/rpc';
import {
  loadMcpServerLayers,
  resolveMcpServersForEngine,
  type StoredMcpServerConfig,
} from './mcp-config';
import {
  beginMcpOAuthFlow,
  cancelMcpOAuthFlow,
  completeMcpOAuthFlow,
} from './mcp-oauth';
import type {
  AgentContextData,
  AgentMeta,
  AgentType,
  BundledSkillActivation,
  ClientPromptMetadata,
  ImportCustomRegistryOptions,
  ImportCustomRegistryResult,
  JsonObject,
  PromptOrigin,
  ResumedAgentState,
  SkillActivationOrigin,
  SkillSource,
  SuggestFilesInput,
  SuggestFilesItem,
  SuggestFilesResult,
  ToolCall,
  AddAdditionalDirInput,
  AddAdditionalDirResult,
  AgentCommandInfo,
  AppMcpServerInspection,
  BackgroundTaskInfo,
  BeginGlobalMcpServerAuthResult,
  CompactOptions,
  ConfigDiagnostics,
  CreateGoalInput,
  CreateSessionOptions,
  ExperimentalFeatureState,
  ExportSessionInput,
  ExportSessionResult,
  FileMeta,
  ForkSessionInput,
  GenerateSessionTitleInput,
  GetConfigOptions,
  GetCronTasksResult,
  GlobalMcpServerAuthStatus,
  GoalSnapshot,
  GoalStatus,
  GoalToolResult,
  KimiHostIdentity,
  ListSessionsOptions,
  McpManagedServerInfo,
  McpServerConfig,
  McpServerInfo,
  McpServerLocator,
  McpTestResult,
  PluginCommandDef,
  PluginInfo,
  PluginSummary,
  PromptPart,
  ReloadSummary,
  RenameSessionInput,
  ResumeSessionInput,
  ResumedSessionSummary,
  SessionPlan,
  SessionStatus,
  SessionSummary,
  SessionSummaryPage,
  SessionTodoItem,
  SessionUsage,
  SkillSummary,
  TelemetryClient,
  UploadFileOptions,
  WorkspaceTrustInfo,
  WorkspaceTrustMcpServerInfo,
} from '#/types';

import {
  BUILTIN_NORMIFY_PLUGIN_ID,
  materializeBuiltinPlugins,
  setBuiltinPluginOptOut,
} from './builtin-normify';
import { lookupModelAlias, probeShellPath, resolveModelContextWindow } from './native-llm-resolver';
import { buildSessionHandle, type BuildHandleDeps } from './sdk-client/build-handle';
import {
  toTurnEndReason,
  toGoalSnapshot,
  nativeGoalWallClockMs,
  computeUndoCut,
  formatUndoUnavailable,
  isUndoAnchorPrompt,
  initialRuntimeState,
  isPlainObject,
  registryKeyFromExisting,
  truncateUpstreamMessage,
  assignOrDelete,
  shellCommandKey,
  retainThroughTurn,
  deepMergeConfigValue,
  posixPath,
  expandCommandArguments,
  normalizeRequiredWorkDir,
  workspaceTrustKey,
  assertSessionIdSegment,
  MAX_TITLE_LENGTH,
  promptMetadataTextFromText,
  promptMetadataTextFromPrompt,
  isUntitledTitle,
  SKILL_SOURCES,
  originClientMetadata,
  displayTextForUndoOrForkLabel,
  buildImportContextParts,
  assertImportFits,
  resolveExperimentalFeatures,
  DEFAULT_INIT_PROMPT,
  resolvePluginMarketplaceDir,
  pluginNodeRunnerPath,
  manifestAgentPaths,
  discoverSubagentsFromToolResult,
} from './sdk-client/helpers';
import type {
  ResolvedSkillPrompt,
  SDKRpcClientNativeOptions,
  NativeSessionMeta,
  NativeGoalState,
  EngineTaskWireEntry,
  PersistedSessionMeta,
} from './sdk-client/helpers';

export class SDKRpcClientNative extends SDKRpcClientBase {
  readonly homeDir: string;
  readonly configPath: string;
  /** Data dir of the app-scope engine store (`<dir>/sessions.db`). */
  readonly engineDataDir: string;
  readonly identity: KimiHostIdentity | undefined;
  readonly telemetry: TelemetryClient;
  readonly auth: KimiAuthFacade;
  readonly skillDirs: readonly string[];

  private readonly liveSessions = new Map<string, NativeSessionMeta>();

  /**
   * Bind the session's log sink. Idempotent per meta, so a create followed by
   * a resume of the same id cannot raise the sink's reference count twice and
   * then leak it on the first close.
   */
  private attachSessionLog(meta: NativeSessionMeta): void {
    if (meta.logHandle !== undefined) return;
    meta.logHandle = getRootLogger().attachSession({
      sessionId: meta.id,
      sessionDir: meta.sessionDir,
    });
  }

  /** Release it; a failure to flush must not stop the session from closing. */
  private async detachSessionLog(meta: NativeSessionMeta): Promise<void> {
    const handle = meta.logHandle;
    meta.logHandle = undefined;
    if (handle === undefined) return;
    await handle.close().catch(() => {});
  }
  private readonly sessionBaseDir: string;
  /** Set once `initPluginStore` has opened the engine's plugin registry. */
  private pluginStoreReady = false;
  /**
   * The shell the Bash tool runs, probed once per client. The probe spawns
   * `where` (pwsh -> powershell), ~100 ms on Windows, and every session
   * creation plus each setModel / setThinking / setPermission handle rebuild
   * would otherwise pay it again. PATH cannot change under a running process.
   */
  private shellPathResolved = false;
  private shellPath: string | undefined;
  /** Sessions whose in-flight compaction was cancelled from the host. */
  private readonly compactionCancels = new Set<string>();
  /**
   * Managed bash handles for live `!` shell commands, keyed by
   * `shellCommandKey(sessionId, commandId)`. `runShellCommand` publishes the
   * handle here so `cancelShellCommand` can kill the process tree it spawned.
   */
  private readonly liveShellCommands = new Map<string, number>();

  constructor(options: SDKRpcClientNativeOptions = {}) {
    super();
    // The engine seeds its client identity / request headers from the host
    // identity, so a harness without one fails at construction like the v2
    // client did (the v1 client tolerated its absence).
    this.identity = assertKimiHostIdentity(options.identity);
    this.homeDir = resolveKimiHome(options.homeDir);
    this.configPath = resolveConfigPath({ homeDir: this.homeDir, configPath: options.configPath });
    this.engineDataDir = options.engineDataDir ?? join(this.homeDir, 'agent');
    this.telemetry = options.telemetry ?? { track: () => {} };
    this.skillDirs = options.skillDirs ?? [];
    this.auth =
      options.auth ??
      new KimiAuthFacade({
        homeDir: this.homeDir,
        configPath: this.configPath,
        identity: this.identity,
        onRefresh: options.onOAuthRefresh,
      });
    this.sessionBaseDir = posixPath(join(this.homeDir, 'sessions'));
    if (!existsSync(this.sessionBaseDir)) {
      mkdirSync(this.sessionBaseDir, { recursive: true });
    }
    void getRootLogger().configure(
      resolveLoggingConfig({ homeDir: this.homeDir, env: process.env }),
    );
  }

  // oxlint-disable-next-line typescript/no-explicit-any
  protected override async getRpc(): Promise<any> {
    // The base class routes ~88 methods through `(await this.getRpc()).<wireName>(…)`.
    // Returning `this` made every not-yet-overridden wire name either throw a
    // TypeError (the wire name is not a method here, e.g. removeKimiProvider /
    // enterPlan / beginCompaction) or recurse forever (the wire name equals a base
    // method that re-enters getRpc, e.g. renameSession / forkSession / undoHistory),
    // hanging the CLI at 100% CPU. Hand back a Proxy that fails loud instead: any
    // wire call this harness has not implemented throws NOT_IMPLEMENTED naming the
    // wire method. Overridden public methods never reach getRpc, so they are
    // unaffected; as later batches override more methods, fewer calls hit this guard.
    return new Proxy(
      {},
      {
        get: (_target, prop) => {
          // `await` probes `then` on whatever it is handed, and this proxy
          // answers every property with a throwing function — so the probe
          // itself threw and every unimplemented call reported `"then"`
          // instead of the method the caller asked for. Answering `then` with
          // `undefined` keeps the proxy a non-thenable, and the real property
          // access below is what throws.
          if (prop === 'then') return undefined;
          throw new KimiError(
            ErrorCodes.NOT_IMPLEMENTED,
            `native harness has not wired RPC method "${String(prop)}"`,
          );
        },
      },
    );
  }

  override async createSession(input: CreateSessionOptions): Promise<SessionSummary> {
    const workDir = normalizeRequiredWorkDir('createSession', input.workDir);
    // A client-chosen id becomes the session directory name; reject anything
    // that is not a single path segment before it reaches join().
    if (input.id !== undefined) assertSessionIdSegment(input.id);
    // NOTE: ensurePluginStore is deliberately NOT called here. Opening the
    // registry takes a SQLite handle on `<engineDataDir>/sessions.db` that
    // Windows keeps locked until closePluginStore — a headless `kimi -p` run
    // that never touches the plugin surface must not leave the data dir
    // locked for its whole lifetime (the recorded upstream decision). The
    // race — a session created before any plugin API ran, missing plugin
    // MCP servers — is closed at the interactive host layer instead
    // (`kimi-tui.ts` sequences refreshPluginCommands before its session
    // prewarm), where the registry is opened on every launch regardless.
    const sessionId = input.id ? input.id : `session_${randomUUID()}`;
    const sessionDir = posixPath(join(this.sessionBaseDir, sessionId));
    const now = Date.now();

    if (
      this.liveSessions.has(sessionId) ||
      this.loadMeta(join(this.sessionBaseDir, sessionId)) !== undefined
    ) {
      throw new KimiError(
        ErrorCodes.SESSION_ALREADY_EXISTS,
        `Session "${sessionId}" already exists`,
      );
    }

    const config = loadRuntimeConfigLenient(this.configPath);
    const { explicit, project } = await this.resolveSessionAdditionalDirs(
      workDir,
      input.additionalDirs ?? [],
    );

    const meta: NativeSessionMeta = {
      id: sessionId,
      workDir,
      sessionDir,
      createdAt: now,
      updatedAt: now,
      title: 'New Session',
      isCustomTitle: false,
      titleKind: 'default',
      lastPrompt: undefined,
      busy: false,
      messageCount: 0,
      promptMetadata: [],
      sessionApprovals: [],
      custom: input.metadata !== undefined ? { ...input.metadata } : {},
      additionalDirs: explicit,
      projectAdditionalDirs: project,
      nonInteractive: input.nonInteractive === true,
      ...initialRuntimeState(config, input.model ?? config.defaultModel),
      plan: undefined,
      forkedFrom: undefined,
      agentProfile: input.agentProfile,
      agentFiles: input.agentFiles?.length ? [...input.agentFiles] : undefined,
    };
    if (input.thinking !== undefined) meta.thinkingEffort = input.thinking;
    if (input.permission !== undefined) meta.permissionMode = input.permission;
    this.liveSessions.set(sessionId, meta);
    this.attachSessionLog(meta);

    try {
      // An unresolvable `--agent` must fail loudly at creation: silently
      // starting the default agent looks like the flag was honoured.
      await this.assertAgentProfileResolves(meta);
      meta.handle = await this.buildHandle(meta);
    } catch (error) {
      // Do not leave a handle-less session advertised as live.
      this.liveSessions.delete(sessionId);
      await this.detachSessionLog(meta);
      throw error;
    }
    this.persistMeta(meta);

    return {
      id: sessionId,
      workDir,
      sessionDir,
      title: meta.title,
      createdAt: now,
      updatedAt: now,
      // v1 returns the caller's metadata verbatim on create (not the merged
      // custom map a later listing would report).
      metadata: input.metadata,
    };
  }

  /**
   * Build the engine session handle for a session from its current meta. Also
   * the rebuild path: a mid-session model / thinking / permission change tears
   * the handle down and re-creates it here, carrying the conversation over via
   * getHistory / setHistory (see rebuildHandle). The callbacks close over meta
   * so the state / goal bridges serve the live session state per turn.
   */
  private async buildHandle(meta: NativeSessionMeta): Promise<EngineSessionHandle> {
    return buildSessionHandle(this.buildHandleDeps(), meta);
  }

  private resolvedShellPath(): string | undefined {
    if (!this.shellPathResolved) {
      this.shellPath = probeShellPath();
      this.shellPathResolved = true;
    }
    return this.shellPath;
  }

  /** Everything `buildSessionHandle` reads from the client, named in one place. */
  private buildHandleDeps(): BuildHandleDeps {
    return {
      homeDir: this.homeDir,
      configPath: this.configPath,
      shellPath: this.resolvedShellPath(),
      identity: this.identity,
      telemetry: this.telemetry,
      auth: this.auth,
      liveSessions: this.liveSessions,
      receiveEvent: (event) => this.receiveEvent(event),
      requestApproval: (request) => this.requestApproval(request),
      requestQuestion: (request) => this.requestQuestion(request),
      persistMeta: (meta) => this.persistMeta(meta),
      requireSession: (sessionId) => this.requireSession(sessionId),
      authorizedRoots: (meta) => this.authorizedRoots(meta),
      refreshProjectRoots: (meta) => this.refreshProjectRoots(meta),
      resolveSessionAgentProfiles: (meta) => this.resolveSessionAgentProfiles(meta),
      setPlanMode: (input) => this.setPlanMode(input),
      loadSessionMcpConfig: (workDir) => this.loadSessionMcpConfig(workDir),
      mcpOAuthDir: this.mcpOAuthDir(),
    };
  }

  override async resumeSession(input: ResumeSessionInput): Promise<ResumedSessionSummary> {
    const sessionId = input.id;
    assertSessionIdSegment(sessionId);
    let meta = this.liveSessions.get(sessionId);
    if (meta === undefined) {
      const now = Date.now();
      const sessionDir = posixPath(join(this.sessionBaseDir, sessionId));
      // Rehydrate the session's own bookkeeping from disk so a rename / metadata
      // update / add-dir made earlier in this home survives the resume, then
      // rebuild the engine handle and replay the persisted history so turns
      // continue with their context.
      const persisted = this.loadMeta(sessionDir);
      const config = loadRuntimeConfigLenient(this.configPath);
      const defaults = initialRuntimeState(config, persisted?.model ?? config.defaultModel);
      const workDir =
        persisted?.workDir ?? normalizeRequiredWorkDir('resumeSession', process.cwd());
      // Only the caller's own roots come back from the session file; the
      // project's are re-read under the *current* trust state, so a workspace
      // that lost trust stops contributing them.
      const { explicit, project } = await this.resolveSessionAdditionalDirs(workDir, [
        ...(persisted?.additionalDirs ?? []),
        ...(input.additionalDirs ?? []),
      ]);
      const created: NativeSessionMeta = {
        id: sessionId,
        workDir,
        sessionDir,
        createdAt: persisted?.createdAt ?? now,
        updatedAt: persisted?.updatedAt ?? now,
        title: persisted?.title ?? 'Resumed Session',
        isCustomTitle: persisted?.isCustomTitle ?? false,
        titleKind: persisted?.isCustomTitle === true ? 'custom' : 'default',
        lastPrompt: persisted?.lastPrompt,
        busy: false,
        messageCount: 0,
        currentTurnId: 0,
        promptMetadata: [],
        sessionApprovals: [],
        custom: persisted?.custom ?? {},
        additionalDirs: explicit,
        projectAdditionalDirs: project,
        nonInteractive: persisted?.nonInteractive ?? false,
        model: persisted?.model ?? config.defaultModel,
        thinkingEffort: persisted?.thinkingEffort ?? defaults.thinkingEffort,
        permissionMode: persisted?.permissionMode ?? defaults.permissionMode,
        planMode: persisted?.planMode ?? false,
        swarmMode: false,
        swarmModeTrigger: 'manual',
        towerMode: false,
        maxContextTokens: resolveModelContextWindow(config, config.defaultModel),
        contextTokens: persisted?.contextTokens ?? 0,
        usageJson: persisted?.usageJson,
        goal: persisted?.goal ?? null,
        plan: persisted?.plan,
        forkedFrom: persisted?.forkedFrom,
        agents: persisted?.agents,
        // The agent binding is the session's identity, fixed at its first
        // bind: resume restores it and ignores a differing requested profile
        // (`docs/en/customization/agents.md:143`).
        agentProfile: persisted?.agentProfile,
        agentFiles: persisted?.agentFiles,
      };
      meta = created;
      this.liveSessions.set(sessionId, created);
      this.attachSessionLog(created);
      // A binding is validated at first create; on resume it is restored, not
      // re-litigated. A profile whose file has since been deleted warns and
      // falls back to the engine default rather than bricking the session.
      try {
        await this.assertAgentProfileResolves(created);
      } catch (error) {
        log.warn(
          `resuming session "${sessionId}" with an unresolvable agent profile; using the default`,
          { error: error instanceof Error ? error.message : String(error) },
        );
        delete created.agentProfile;
      }
      created.handle = await this.buildHandle(created);
      const history = this.readPersistedHistory(created);
      if (history.length > 0) {
        await created.handle.setHistory(history);
        created.messageCount = history.length;
      }
    } else if (input.additionalDirs !== undefined && input.additionalDirs.length > 0) {
      // A live session still honors a resume that carries roots: the caller's
      // list (plus the project's, when trusted) is authorized the same way
      // `/add-dir` authorizes one, handle rebuild included.
      await this.authorizeAdditionalDirs(meta, input.additionalDirs);
    }
    return this.resumedSessionSummary(meta, input);
  }

  /**
   * v2 `WorkspaceDirsService`: a session's extra roots are the roots the caller
   * asked for (`ephemeralDirs`) plus the project's `workspace.additional_dir`
   * (`fileDirs`), and the project's half is gated on workspace trust — an
   * untrusted checkout cannot widen its own sandbox with a file it ships.
   *
   * The two halves stay in separate fields (v2 keeps the same split) so the
   * project half is re-derived on every create/resume/rebuild and never
   * persists: revoking trust withdraws it, and it can never be handed back to
   * the engine as if the caller had asked for it.
   *
   * The caller's own roots go through the same resolver the project file uses,
   * so `~`, relative paths, missing directories and broad-scope roots are judged
   * once, at the workspace, before anything is authorized (v2
   * `mergeAdditionalDirs` → `resolveAdditionalDirs`).
   */
  private async resolveSessionAdditionalDirs(
    workDir: string,
    requested: readonly string[],
  ): Promise<{ explicit: string[]; project: string[] }> {
    const explicit = await this.resolveCallerDirs(workDir, requested);
    const { trusted } = await this.getWorkspaceTrustInfo(workDir);
    if (!trusted) return { explicit, project: [] };
    try {
      const { additionalDirs } = await readAdditionalDirs(workDir);
      return { explicit, project: [...additionalDirs] };
    } catch (error) {
      log.warn('ignoring the project-local config of an unreadable workspace', {
        workDir,
        error: error instanceof Error ? error.message : String(error),
      });
      return { explicit, project: [] };
    }
  }

  /**
   * The caller's roots, resolved against the workspace. One unusable entry is
   * reported and skipped rather than taking the whole session's roots with it:
   * a stale `/add-dir` from an earlier session must not block starting work.
   */
  private async resolveCallerDirs(
    workDir: string,
    requested: readonly string[],
  ): Promise<string[]> {
    const resolved: string[] = [];
    for (const dir of requested) {
      try {
        for (const one of await resolveAdditionalDirs(workDir, [dir])) {
          if (!resolved.includes(one)) resolved.push(one);
        }
      } catch (error) {
        log.warn('ignoring an additional directory that cannot be authorized', {
          workDir,
          dir,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    }
    return resolved;
  }

  /**
   * Every root the engine is authorized for: the caller's, then the project's.
   */
  private authorizedRoots(meta: NativeSessionMeta): string[] {
    return [...new Set([...meta.additionalDirs, ...meta.projectAdditionalDirs])];
  }

  /**
   * Authorize `requested` roots for a live session, rebuilding the engine
   * handle so the sandbox and the directory listing both see them. A failed
   * rebuild leaves the previous roots in place.
   */
  private async authorizeAdditionalDirs(
    meta: NativeSessionMeta,
    requested: readonly string[],
  ): Promise<void> {
    const previous = meta.additionalDirs;
    // Only the caller's own roots are authorized here; the project's half is
    // re-read by `buildHandle` so a trust change takes effect on the rebuild.
    const { explicit } = await this.resolveSessionAdditionalDirs(meta.workDir, [
      ...previous,
      ...requested,
    ]);
    if (explicit.length === previous.length) return;
    meta.additionalDirs = explicit;
    await this.refreshProjectRoots(meta);
    try {
      await this.rebuildHandle(meta);
    } catch (error) {
      meta.additionalDirs = previous;
      throw error;
    }
    // `rebuildHandle` already stamped `meta.updatedAt`.
    this.persistMeta(meta);
  }

  /**
   * Re-derive the project's roots for `meta` from the current trust state.
   * Called before every handle build, so trusting or revoking a workspace
   * takes effect without recreating the session.
   */
  private async refreshProjectRoots(meta: NativeSessionMeta): Promise<void> {
    const { project } = await this.resolveSessionAdditionalDirs(meta.workDir, []);
    meta.projectAdditionalDirs = project;
  }

  /** The `ResumedSessionSummary` of a session, including the per-agent main snapshot and any subagents. */
  private async resumedSessionSummary(
    meta: NativeSessionMeta,
    input?: ResumeSessionInput,
  ): Promise<ResumedSessionSummary> {
    let history = meta.handle ? await meta.handle.getHistory().catch(() => []) : [];
    if (history.length === 0) {
      history = this.readPersistedHistory(meta);
    }
    const context = this.contextFromHistory(meta, history);

    const sessionAgentsRoster: Record<string, AgentMeta> = {
      main: { homedir: meta.workDir, type: 'main' as const, parentAgentId: null },
      ...meta.agents,
    };

    const discoveredSubagents = new Map<
      string,
      {
        type: AgentType;
        prompt: string;
        summary: string;
        startedAt: number;
      }
    >();

    const calls = new Map<string, { name: string; prompt: string; startedAt: number }>();

    for (let i = 0; i < history.length; i++) {
      const msg = history[i]!;
      const raw = msg as unknown as Record<string, unknown>;
      const time = meta.createdAt + i;
      const rawCalls = (raw['tool_calls'] ?? raw['toolCalls']) as unknown[] | undefined;
      if (Array.isArray(rawCalls)) {
        for (const c of rawCalls) {
          const call = (c && typeof c === 'object' ? c : {}) as Record<string, unknown>;
          const name = typeof call['name'] === 'string' ? call['name'] : '';
          const id = typeof call['id'] === 'string' ? call['id'] : '';
          if (name === 'Agent' || name === 'AgentSwarm') {
            let prompt = '';
            try {
              const rawArgs = call['arguments'];
              const args =
                typeof rawArgs === 'string'
                  ? (JSON.parse(rawArgs) as Record<string, unknown>)
                  : (rawArgs as Record<string, unknown>);
              if (args && typeof args === 'object') {
                prompt = typeof args['prompt'] === 'string' ? args['prompt'] : '';
              }
            } catch {}
            calls.set(id, { name, prompt, startedAt: time });
          }
        }
      }

      const rawCallId = raw['tool_call_id'] ?? raw['toolCallId'];
      const toolCallId = typeof rawCallId === 'string' ? rawCallId : '';
      if (toolCallId && calls.has(toolCallId)) {
        const callInfo = calls.get(toolCallId)!;
        const content = typeof msg.content === 'string' ? msg.content : '';
        for (const found of discoverSubagentsFromToolResult(content)) {
          sessionAgentsRoster[found.agentId] = {
            homedir: meta.workDir,
            type: 'sub',
            parentAgentId: 'main',
          };
          discoveredSubagents.set(found.agentId, {
            type: 'sub',
            prompt: callInfo.prompt,
            summary: found.summary,
            startedAt: callInfo.startedAt,
          });
        }
      }
    }

    const resumedAgents: Record<string, ResumedAgentState> = {
      main: {
        type: 'main',
        config: {
          cwd: meta.workDir,
          modelAlias: meta.model,
          modelCapabilities: {
            image_in: false,
            video_in: false,
            audio_in: false,
            thinking: false,
            tool_use: true,
            max_context_tokens: meta.maxContextTokens,
          },
          thinkingEffort: meta.thinkingEffort,
          systemPrompt: '',
        },
        context,
        replay: history.map((message, index) => ({
          type: 'message' as const,
          time: meta.createdAt + index,
          message: context.history[index]!,
        })),
        permission: { mode: meta.permissionMode },
        plan: meta.plan
          ? { id: meta.plan.id, content: meta.plan.content, path: meta.plan.path }
          : null,
        swarmMode: meta.swarmMode,
        usage: { ...this.usageFromSnapshot(meta.usageJson) },
        tools: [],
        background: [],
      },
    };

    if (input?.includeSubagents === true) {
      for (const [childAgentId, sub] of discoveredSubagents) {
        resumedAgents[childAgentId] = {
          type: sub.type,
          config: {
            cwd: meta.workDir,
            modelAlias: meta.model,
            modelCapabilities: {
              image_in: false,
              video_in: false,
              audio_in: false,
              thinking: false,
              tool_use: true,
              max_context_tokens: meta.maxContextTokens,
            },
            thinkingEffort: meta.thinkingEffort,
            systemPrompt: '',
          },
          context: {
            history: [
              {
                id: `${childAgentId}_prompt`,
                role: 'user',
                content: [{ type: 'text', text: sub.prompt }],
                origin: { kind: 'user' },
              },
              {
                id: `${childAgentId}_reply`,
                role: 'assistant',
                content: [{ type: 'text', text: sub.summary }],
              },
            ],
            tokenCount: 0,
          },
          replay: [
            {
              type: 'message',
              time: sub.startedAt,
              message: {
                id: `${childAgentId}_prompt`,
                role: 'user',
                content: [{ type: 'text', text: sub.prompt }],
                origin: { kind: 'user' },
              },
            },
            {
              type: 'message',
              time: sub.startedAt + 1,
              message: {
                id: `${childAgentId}_reply`,
                role: 'assistant',
                content: [{ type: 'text', text: sub.summary }],
              },
            },
          ],
          permission: { mode: meta.permissionMode },
          plan: null,
          swarmMode: false,
          usage: { inputOther: 0, output: 0, inputCacheRead: 0, inputCacheCreation: 0 },
          tools: [],
          background: [],
        };
      }
    }

    return {
      id: meta.id,
      workDir: meta.workDir,
      sessionDir: meta.sessionDir,
      title: meta.title,
      createdAt: meta.createdAt,
      updatedAt: meta.updatedAt,
      metadata: { ...meta.custom } as JsonObject,
      // What the session authorizes, which includes the project's roots while
      // the workspace is trusted (v2 reports `fileDirs ∪ ephemeralDirs`).
      additionalDirs: this.authorizedRoots(meta),
      lastPrompt: meta.lastPrompt,
      sessionMetadata: {
        createdAt: new Date(meta.createdAt).toISOString(),
        updatedAt: new Date(meta.updatedAt).toISOString(),
        title: meta.title,
        isCustomTitle: meta.isCustomTitle,
        workDir: meta.workDir,
        agents: sessionAgentsRoster,
        custom: { ...meta.custom } as JsonObject,
      },
      agents: resumedAgents,
    };
  }

  override async renameSession(input: RenameSessionInput): Promise<void> {
    assertSessionIdSegment(input.id);
    const title = input.title.trim();
    if (title.length === 0) {
      throw new KimiError(ErrorCodes.SESSION_TITLE_EMPTY, 'Session title cannot be empty');
    }
    const meta = this.liveSessions.get(input.id);
    if (meta !== undefined) {
      meta.title = title;
      meta.isCustomTitle = true;
      meta.titleKind = 'custom';
      meta.updatedAt = Date.now();
      this.persistMeta(meta);
      return;
    }
    // A closed session is renamed at the store level: load its persisted
    // bookkeeping, update the title, and write it back.
    const sessionDir = join(this.sessionBaseDir, input.id);
    const persisted = this.loadMeta(sessionDir);
    if (persisted === undefined) {
      throw new KimiError(ErrorCodes.SESSION_NOT_FOUND, `unknown session "${input.id}"`, {
        details: { sessionId: input.id },
      });
    }
    persisted.title = title;
    persisted.isCustomTitle = true;
    persisted.updatedAt = Date.now();
    this.writePersistedMeta(sessionDir, persisted);
  }

  override async updateSessionMetadata(input: UpdateSessionMetadataRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    // v2 merges the patch into `session.custom` rather than replacing it.
    meta.custom = { ...meta.custom, ...input.metadata };
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
  }

  override async addAdditionalDir(input: AddAdditionalDirInput): Promise<AddAdditionalDirResult> {
    const meta = this.requireSession(input.id);
    // v2 resolves the argument against the workspace before it is stored, so
    // the session and the file agree on one spelling of the same directory —
    // and so `~`, a relative path, a missing directory or a broad-scope root
    // is rejected here rather than authorized. Resolution either yields exactly
    // one path or throws, so the fallback is unreachable.
    const [resolvedPath] = await resolveAdditionalDirs(meta.workDir, [input.path]);
    const path = resolvedPath ?? input.path;
    const changed = !meta.additionalDirs.includes(path);
    if (changed) {
      // The extra roots are baked into the engine handle at build time — the
      // native toolset's sandbox is constructed from them — so a newly
      // authorized directory only takes effect after a rebuild. Same contract as
      // setModel / setPermission: carry the history over, and drop the root
      // again when the rebuild fails.
      const previous = meta.additionalDirs;
      meta.additionalDirs = [...previous, path];
      try {
        await this.rebuildHandle(meta);
      } catch (error) {
        meta.additionalDirs = previous;
        throw error;
      }
      // `rebuildHandle` already stamped `meta.updatedAt`.
      this.persistMeta(meta);
    }
    // `persist` writes the root into the project's `.kimi-code/local.toml`, so
    // every later session of this checkout starts with it — the promise
    // `docs/*/configuration/config-files.md` makes for "remember this
    // directory". The write comes *after* the rebuild: a rebuild that failed
    // must not leave a remembered directory behind that no session authorizes.
    const persisted =
      input.persist === true ? await appendAdditionalDir(meta.workDir, path) : undefined;
    // What this session actually authorizes, never what the file happens to
    // list: an untrusted checkout's `local.toml` is not the caller's business,
    // and its entries must not travel back as if they had been requested here
    // (they would return as "explicit" roots on the next session).
    const additionalDirs = [...new Set([...this.authorizedRoots(meta), ...meta.additionalDirs])];
    return {
      additionalDirs,
      projectRoot: persisted?.projectRoot ?? meta.workDir,
      configPath: persisted?.configPath ?? this.configPath,
      persisted: persisted !== undefined,
    };
  }

  override async listSessions(input: ListSessionsOptions = {}): Promise<readonly SessionSummary[]> {
    const workDir =
      input.workDir === undefined
        ? undefined
        : normalizeRequiredWorkDir('listSessions', input.workDir);
    const sessionsMap = new Map<string, SessionSummary>();
    // The project's roots are per workspace, not per session: read them once per
    // distinct work dir instead of once per session, and never persist them —
    // a stored copy would keep authorizing a workspace that lost trust.
    const storedMetas: PersistedSessionMeta[] = [];
    if (existsSync(this.sessionBaseDir)) {
      try {
        const dirs = readdirSync(this.sessionBaseDir, { withFileTypes: true });
        for (const dir of dirs) {
          if (!dir.isDirectory()) continue;
          const meta = this.loadMeta(join(this.sessionBaseDir, dir.name));
          if (meta !== undefined) storedMetas.push(meta);
        }
      } catch {
        // ignore
      }
    }
    const projectRootsByWorkDir = new Map<string, string[]>();
    for (const meta of storedMetas) {
      if (projectRootsByWorkDir.has(meta.workDir)) continue;
      const { project } = await this.resolveSessionAdditionalDirs(meta.workDir, []);
      projectRootsByWorkDir.set(meta.workDir, project);
    }
    for (const meta of storedMetas) {
      sessionsMap.set(meta.id, {
        id: meta.id,
        workDir: meta.workDir,
        sessionDir: posixPath(join(this.sessionBaseDir, meta.id)),
        title: meta.title,
        createdAt: meta.createdAt,
        updatedAt: meta.updatedAt,
        lastPrompt: meta.lastPrompt,
        metadata: { ...meta.custom } as JsonObject,
        additionalDirs: [
          ...new Set([...meta.additionalDirs, ...(projectRootsByWorkDir.get(meta.workDir) ?? [])]),
        ],
      });
    }
    for (const meta of this.liveSessions.values()) {
      sessionsMap.set(meta.id, {
        id: meta.id,
        workDir: meta.workDir,
        sessionDir: meta.sessionDir,
        title: meta.title,
        createdAt: meta.createdAt,
        updatedAt: meta.updatedAt,
        lastPrompt: meta.lastPrompt,
        metadata: { ...meta.custom } as JsonObject,
        additionalDirs: this.authorizedRoots(meta),
      });
    }
    const all = Array.from(sessionsMap.values()).sort(
      (a, b) => b.updatedAt - a.updatedAt || b.createdAt - a.createdAt || (a.id < b.id ? 1 : -1),
    );
    return all.filter((session) => {
      if (input.sessionId !== undefined && session.id !== input.sessionId) return false;
      if (workDir !== undefined && session.workDir !== workDir) return false;
      return true;
    });
  }

  override async listSessionsPage(input: ListSessionsOptions = {}): Promise<SessionSummaryPage> {
    // Keyset pagination over the same ordering `listSessions` serves: `before`
    // is the last id of the previous page and the next page holds the entries
    // strictly older than it. An unknown cursor answers an empty terminal page.
    const items = await this.listSessions(input);
    const limit = input.limit;
    if (limit === undefined && input.before === undefined) {
      return { items };
    }
    const startIndex =
      input.before === undefined ? 0 : items.findIndex((item) => item.id === input.before) + 1;
    if (startIndex <= 0 && input.before !== undefined) {
      return { items: [] };
    }
    const window =
      limit === undefined ? items.slice(startIndex) : items.slice(startIndex, startIndex + limit);
    const exhausted = startIndex + window.length >= items.length;
    return {
      items: window,
      ...(exhausted || window.length === 0 ? {} : { nextCursor: window.at(-1)?.id }),
    };
  }

  override async deleteSession(input: SessionIdRpcInput): Promise<void> {
    // The id is removed with `rmSync(recursive)`: an unchecked segment would
    // delete a directory outside the sessions root.
    assertSessionIdSegment(input.sessionId);
    const meta = this.liveSessions.get(input.sessionId);
    if (
      meta === undefined &&
      this.loadMeta(join(this.sessionBaseDir, input.sessionId)) === undefined
    ) {
      throw new KimiError(ErrorCodes.SESSION_NOT_FOUND, `unknown session "${input.sessionId}"`, {
        details: { sessionId: input.sessionId },
      });
    }
    if (meta?.handle) {
      await meta.handle.dispose().catch(() => {});
    }
    this.liveSessions.delete(input.sessionId);
    if (meta !== undefined) await this.detachSessionLog(meta);
    // The live handle is gone, but the directory is what `listSessions`
    // enumerates: swallowing a failed removal told the caller the delete
    // succeeded while the session reappeared in the picker on the next list.
    // The TUI reports the rejection ("Failed to delete session …"), and a
    // retry finds the session on disk again and removes it.
    rmSync(join(this.sessionBaseDir, input.sessionId), { recursive: true, force: true });
  }

  override async closeSession(input: SessionIdRpcInput): Promise<void> {
    const meta = this.liveSessions.get(input.sessionId);
    if (meta?.handle) {
      await this.persistHistory(meta).catch(() => {});
      await meta.handle.dispose().catch(() => {});
    }
    // Drop it from the live table: leaving a disposed handle behind made
    // listSessions keep advertising a closed session, and the next prompt would
    // enqueue a turn onto a freed native session id.
    this.liveSessions.delete(input.sessionId);
    if (meta !== undefined) await this.detachSessionLog(meta);
  }

  override async prompt(input: SessionPromptRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    this.assertSessionModelWindow(meta);
    if (!meta.handle) {
      // Never silently drop user input: an unknown/closed session is an error,
      // not a no-op that resolves while the TUI shows nothing.
      throw new KimiError(
        ErrorCodes.SESSION_NOT_FOUND,
        `cannot prompt unknown or closed session "${input.sessionId}"`,
      );
    }
    const prompt = this.toSessionPrompt(
      input.input,
      this.promptOriginFromClientMetadata(input.clientMetadata),
    );
    const agentId = this.interactiveAgentId;
    meta.activeAgentId = agentId;
    // Side-channel (btw) turns run outside the session's turn queue on the
    // subagent instance, so the session-level busy flag and prompt metadata
    // stay untouched (v2 btw semantics).
    if (agentId !== 'main') {
      try {
        await this.runSideChannelTurn(meta, agentId, prompt.content);
      } finally {
        if (meta.activeAgentId === agentId) meta.activeAgentId = undefined;
      }
      return;
    }
    meta.busy = true;
    meta.updatedAt = Date.now();
    // v1/v2 updated the prompt-derived title/lastPrompt before the turn
    // launched; the turn itself fails asynchronously (a model-less turn
    // rejects the turn, not the submission). Subagents (like btw) leave the
    // session-level metadata alone.
    if (!input.skipPromptMetadata) {
      // v2 #3764: this entry's displayText wins for the metadata when the
      // client supplied one; the raw input text is the fallback. The record
      // appended here feeds the all-entries displayText judgment for the
      // undo-label / fork-title derivations.
      this.applyPromptMetadata(
        meta,
        promptMetadataTextFromPrompt(input.input),
        input.clientMetadata?.displayText,
      );
    }
    try {
      const turnId = await meta.handle.enqueueTurn(prompt, 'newTurn');
      void meta.handle.turnOutcome(turnId).then(
        (outcome) => this.settleTurn(meta, outcome),
        () => this.settleTurn(meta, undefined),
      );
    } catch (error) {
      meta.busy = false;
      throw error;
    }
  }

  /**
   * Run one btw side-channel turn on the engine's subagent instance and frame
   * it with turn.started / turn.ended events: the engine's foreground
   * subagent path emits only the streaming deltas (the session pump owns the
   * turn lifecycle events for queue turns), so the SDK supplies the framing
   * the btw panel consumes. Like the main path, a failed turn surfaces
   * through a `turn.ended` failure event instead of rejecting the submission.
   */
  private async runSideChannelTurn(
    meta: NativeSessionMeta,
    agentId: string,
    text: string,
  ): Promise<void> {
    if (!meta.handle) return;
    this.receiveEvent({
      sessionId: meta.id,
      agentId,
      turnId: meta.currentTurnId,
      type: 'turn.started',
      origin: { kind: 'user' },
      prompt: text,
    });
    let reason: TurnEndReason = 'completed';
    try {
      const outcome = await meta.handle.btwPrompt(agentId, text);
      // `EndTurn` / `Aborted` / `MaxTokens` (Rust Debug) → protocol reason.
      reason = toTurnEndReason(outcome.stopReason.replaceAll(/([a-z])([A-Z])/g, '$1_$2'));
    } catch {
      reason = 'failed';
    }
    this.receiveEvent({
      sessionId: meta.id,
      agentId,
      turnId: meta.currentTurnId,
      type: 'turn.ended',
      reason,
    });
  }

  override async cancel(input: SessionIdRpcInput): Promise<void> {
    const meta = this.liveSessions.get(input.sessionId);
    if (!meta?.handle) return;
    // The interactive-agent scope decides which turn the cancel targets: a
    // side-channel (btw) turn is not in the session's turn queue, so its
    // abort goes to the engine's registered parent-cancel for that agent.
    const agentId = this.interactiveAgentId;
    if (agentId !== 'main') {
      await meta.handle.btwCancel(agentId);
      return;
    }
    await meta.handle.cancelTurn();
  }

  override async steer(input: SessionPromptRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    this.assertSessionModelWindow(meta);
    if (!meta.handle) {
      throw new KimiError(
        ErrorCodes.SESSION_NOT_FOUND,
        `cannot steer unknown or closed session "${input.sessionId}"`,
      );
    }
    meta.updatedAt = Date.now();

    const prompt = this.toSessionPrompt(
      input.input,
      this.promptOriginFromClientMetadata(input.clientMetadata),
    );
    // v2 #3764: this entry's displayText wins for the metadata when the
    // client supplied one; the raw input text is the fallback. Skipped when
    // the caller already applied the metadata (the busy skill activation
    // resubmits here after applying its own entry's metadata).
    if (!input.skipPromptMetadata) {
      this.applyPromptMetadata(
        meta,
        promptMetadataTextFromPrompt(input.input),
        input.clientMetadata?.displayText,
      );
    }
    const turnId = await meta.handle.enqueueTurn(prompt, 'activeOrNewTurn');
    if (!meta.busy) {
      meta.busy = true;
      void meta.handle.turnOutcome(turnId).then(
        (outcome) => this.settleTurn(meta, outcome),
        () => this.settleTurn(meta, undefined),
      );
    }
  }

  /**
   * Settle a submitted turn asynchronously: clear the busy flag, fold the
   * outcome into the usage counters, and persist the post-turn history. Turn
   * failures surface through the `turn.ended` event stream, not the
   * submission promise (v1/v2 semantics).
   */
  private settleTurn(meta: NativeSessionMeta, outcome: SessionTurnOutcome | undefined): void {
    meta.busy = false;
    meta.updatedAt = Date.now();
    if (outcome?.result) {
      this.recordTurnOutcome(meta, outcome);
    }
    void this.persistHistory(meta);
  }

  override async getUsage(input: SessionIdRpcInput): Promise<SessionUsage> {
    const meta = this.requireSession(input.sessionId);
    if (meta.handle !== undefined) {
      try {
        const status = await meta.handle.status();
        if (status.usageJson !== undefined && status.usageJson !== null) {
          if (meta.usageJson !== status.usageJson) {
            meta.usageJson = status.usageJson;
            this.persistMeta(meta);
          }
          return this.usageFromSnapshot(status.usageJson);
        }
      } catch {
        // Fall through to the cached snapshot.
      }
    }
    return this.usageFromSnapshot(meta.usageJson);
  }

  /**
   * The engine's `UsageStatus` snapshot as the SDK's `SessionUsage`. The host
   * only decodes the opaque JSON the engine produced; it never derives the
   * buckets or totals itself.
   */
  private usageFromSnapshot(raw: string | undefined): SessionUsage {
    if (raw === undefined) return {};
    try {
      const parsed: unknown = JSON.parse(raw);
      return typeof parsed === 'object' && parsed !== null ? (parsed as SessionUsage) : {};
    } catch {
      return {};
    }
  }

  /** Cache the engine's latest usage snapshot after a turn and persist it. */
  private async refreshUsage(meta: NativeSessionMeta): Promise<void> {
    if (meta.handle === undefined) return;
    try {
      const status = await meta.handle.status();
      if (status.usageJson !== undefined && status.usageJson !== null) {
        meta.usageJson = status.usageJson;
        this.persistMeta(meta);
      }
    } catch {
      // Best-effort: the next turn or getUsage reads the engine again.
    }
  }

  override async getStatus(input: SessionIdRpcInput): Promise<SessionStatus> {
    const meta = this.requireSession(input.sessionId);
    // Deliberately unclamped, same as v2: >100% is the documented overflow
    // signal on this path.
    const contextUsage = meta.maxContextTokens > 0 ? meta.contextTokens / meta.maxContextTokens : 0;
    return {
      model: meta.model,
      thinkingEffort: meta.thinkingEffort,
      permission: meta.permissionMode,
      planMode: meta.planMode,
      swarmMode: meta.swarmMode,
      towerMode: meta.towerMode,
      contextTokens: meta.contextTokens,
      maxContextTokens: meta.maxContextTokens,
      contextUsage,
      usage: await this.getUsage(input),
    };
  }

  override async clearContext(input: SessionIdRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    if (meta.handle) {
      await meta.handle.clearHistory();
    }
    meta.contextTokens = 0;
    meta.messageCount = 0;
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    await this.persistHistory(meta);
  }

  override async setPlanMode(input: SetSessionPlanModeRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    // The engine's plan guard reads this live through the stateRead bridge, so
    // flipping it here takes effect on the next guarded tool call — no handle
    // rebuild, and an in-flight turn sees the new mode at its next guard check.
    const wasPlanMode = meta.planMode;
    meta.planMode = input.enabled;
    if (input.enabled && (!wasPlanMode || meta.plan === undefined)) {
      // Entering plan mode materializes a fresh plan document handle and
      // prepares the plans directory; the plan file itself is only written
      // when content is set (repeated toggles never leave plan files behind).
      const id = `plan_${Date.now()}_${randomUUID().slice(0, 8)}`;
      const plansDir = join(meta.sessionDir, 'agents', 'main', 'plans');
      try {
        mkdirSync(plansDir, { recursive: true });
      } catch {
        // best-effort: the document handle still reports the prepared path
      }
      meta.plan = {
        id,
        content: '',
        path: posixPath(join(plansDir, `${id}.md`)),
      };
    }
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    this.emitStatusUpdated(meta);
  }

  override async getPlan(_input: SessionIdRpcInput): Promise<SessionPlan> {
    const meta = this.requireSession(_input.sessionId);
    if (meta.plan === undefined) return null;
    // The plan document's content lives in the plan file; read it so a
    // host-written plan (or a fork copy) reports its actual content.
    let content = meta.plan.content;
    try {
      content = readFileSync(meta.plan.path, 'utf-8');
    } catch {
      // no plan file yet — keep the in-memory content
    }
    return { id: meta.plan.id, content, path: meta.plan.path };
  }

  override async clearPlan(input: SessionIdRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    meta.planMode = false;
    // Clearing keeps the document handle but resets its content (v1's
    // `clearPlan` emptied the active plan file).
    if (meta.plan !== undefined) {
      meta.plan = { ...meta.plan, content: '' };
      try {
        mkdirSync(dirname(meta.plan.path), { recursive: true });
        writeFileSync(meta.plan.path, '', 'utf8');
      } catch {
        // best-effort: the document handle still reports empty content
      }
    }
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    this.emitStatusUpdated(meta);
  }

  override async importContext(input: ImportContextRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    if (!meta.handle) {
      throw new KimiError(
        ErrorCodes.SESSION_NOT_FOUND,
        `cannot import context into session "${input.sessionId}" without a live engine handle`,
      );
    }
    const parts = buildImportContextParts(input.content, input.source);
    // v1's overflow gate: the import estimate plus the current context must
    // fit the model window (unknown window = 0 skips the check).
    const importTokens = estimateTokensForMessages([
      { role: 'user', content: [...parts], toolCalls: [] },
    ]);
    assertImportFits(importTokens, meta.contextTokens, meta.maxContextTokens);
    await meta.handle.extendHistory([this.toSessionPromptFromParts(parts)]);
    // v1 adopted the post-import estimate as its reported token count.
    meta.contextTokens += importTokens;
    meta.messageCount += 1;
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    await this.persistHistory(meta);
  }

  override async createGoal(input: SessionIdRpcInput & CreateGoalInput): Promise<GoalSnapshot> {
    const meta = this.requireSession(input.sessionId);
    // v2 `prepareForGoalCreation`: any existing goal must be explicitly
    // replaced, whatever its status.
    if (meta.goal && input.replace !== true) {
      throw new KimiError(
        ErrorCodes.GOAL_ALREADY_EXISTS,
        'A goal already exists; use replace to start a new one',
      );
    }
    const now = Date.now();
    meta.goal = {
      goalId: `goal_${randomUUID()}`,
      objective: input.objective,
      status: 'active',
      createdAt: now,
      updatedAt: now,
      turnsUsed: 0,
      tokensUsed: 0,
      inputTokensUsed: 0,
      outputTokensUsed: 0,
      // The engine owns the live wall-clock figure; a fresh goal starts at 0
      // and the next `goal.updated` carries the engine's reading.
      wallClockMs: 0,
      budget: {},
    };
    this.persistMeta(meta);
    // The goal panel is driven by `goal.updated`; nothing else emitted it, so
    // a goal created through the SDK never reached the UI.
    this.emitGoalUpdated(input.sessionId, meta.goal);
    return toGoalSnapshot(meta.goal);
  }

  override async getGoal(input: SessionIdRpcInput): Promise<GoalToolResult> {
    const meta = this.requireSession(input.sessionId);
    return { goal: meta.goal ? toGoalSnapshot(meta.goal) : null };
  }

  override async pauseGoal(input: SessionIdRpcInput): Promise<GoalSnapshot> {
    return this.transitionGoal(input.sessionId, 'paused');
  }

  override async resumeGoal(input: SessionIdRpcInput): Promise<GoalSnapshot> {
    return this.transitionGoal(input.sessionId, 'active');
  }

  override async cancelGoal(input: SessionIdRpcInput): Promise<GoalSnapshot> {
    // v2 `cancelGoal`: the goal is removed, not completed. The returned
    // snapshot is the pre-cancel state; the engine store follows on the next
    // goal-state write, where a host that reports no goal clears it.
    const meta = this.requireSession(input.sessionId);
    const goal = meta.goal;
    if (!goal) {
      throw new KimiError(ErrorCodes.GOAL_NOT_FOUND, 'No current goal');
    }
    const snapshot = toGoalSnapshot(goal);
    meta.goal = null;
    this.persistMeta(meta);
    this.receiveEvent({
      sessionId: input.sessionId,
      agentId: 'main',
      type: 'goal.updated',
      snapshot: null,
    });
    return snapshot;
  }

  private transitionGoal(sessionId: string, status: GoalStatus): GoalSnapshot {
    const meta = this.requireSession(sessionId);
    const goal = meta.goal;
    if (!goal) {
      throw new KimiError(ErrorCodes.GOAL_NOT_FOUND, 'No current goal');
    }
    const now = Date.now();
    if (status === 'active') {
      // v2 `resumeGoal`: only a paused or blocked goal can resume.
      if (goal.status === 'active') return toGoalSnapshot(goal);
      if (goal.status !== 'paused' && goal.status !== 'blocked') {
        throw new KimiError(
          ErrorCodes.GOAL_NOT_RESUMABLE,
          `Cannot resume a goal in status "${goal.status}"`,
        );
      }
      goal.wallClockMs = nativeGoalWallClockMs(goal, now);
      goal.status = 'active';
      goal.updatedAt = now;
    } else {
      // v2 `pauseGoal`: already paused is idempotent; anything else must be
      // active.
      if (goal.status === 'paused') return toGoalSnapshot(goal);
      if (goal.status !== 'active') {
        throw new KimiError(
          ErrorCodes.GOAL_STATUS_INVALID,
          `Cannot pause a goal in status "${goal.status}"`,
        );
      }
      // Settle the live clock at the pause, so the panel stops counting.
      goal.wallClockMs = nativeGoalWallClockMs(goal, now);
      goal.status = status;
      goal.updatedAt = now;
    }
    this.persistMeta(meta);
    this.emitGoalUpdated(sessionId, goal);
    return toGoalSnapshot(goal);
  }

  private emitGoalUpdated(sessionId: string, goal: NativeGoalState): void {
    this.receiveEvent({
      sessionId,
      agentId: 'main',
      type: 'goal.updated',
      snapshot: toGoalSnapshot(goal) as unknown as ProtocolGoalSnapshot,
    });
  }

  override async setModel(input: SetSessionModelRpcInput): Promise<SetSessionModelRpcResult> {
    const meta = this.requireSession(input.sessionId);
    // The native LLM (model / thinking budget) is baked into the engine handle
    // at build time, so a model change rebuilds it, carrying the history over.
    await this.applyRebuiltSetting(meta, 'model', input.model);
    this.emitStatusUpdated(meta);
    // Reached only when the rebuild succeeded, so the handle runs `input.model`.
    return { model: input.model };
  }

  override async setThinking(input: SetSessionThinkingRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    await this.applyRebuiltSetting(meta, 'thinkingEffort', input.effort);
    this.emitStatusUpdated(meta);
  }

  override async setPermission(input: SetSessionPermissionRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    if (meta.permissionMode === input.mode) {
      this.emitStatusUpdated(meta);
      return;
    }
    const previous = meta.permissionMode;
    meta.permissionMode = input.mode;
    // Both attempts are compensated the same way: an unknown mode makes the
    // engine's switch return an error, and a failed rebuild leaves the previous
    // handle in place. Either failure must restore the recorded mode, or the
    // session reports one mode while the engine enforces another.
    try {
      // The engine's PermissionEngine supports a live mode switch, so the new
      // mode reaches the turn already running instead of waiting for the next
      // handle. A session without a live engine falls back to a rebuild.
      const switchedLive = meta.handle ? await meta.handle.setPermissionMode(input.mode) : false;
      if (!switchedLive) {
        await this.rebuildHandle(meta);
      }
    } catch (error) {
      meta.permissionMode = previous;
      throw error;
    }
    this.persistMeta(meta);
    this.emitStatusUpdated(meta);
  }

  override async setTowerMode(input: SetSessionTowerModeRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    // The tower tools run engine-side natively (tools/tower), gated on the
    // `main` caller and the `.tower/` workspace state — not on this flag. The
    // flag records the coordinator mode for the host (steering semantics +
    // status display); the requested base is validated engine-side when the
    // agent runs TowerInit.
    const wasOn = meta.towerMode;
    meta.towerMode = input.enabled;
    meta.updatedAt = Date.now();
    // v2 #3897 `tower_mode_enter` / `tower_mode_exit`: emitted on the
    // transition, from the host side — the flag lives here, and the engine has
    // no flip point of its own to observe.
    if (wasOn !== input.enabled) {
      this.telemetry.track(input.enabled ? 'tower_mode_enter' : 'tower_mode_exit');
    }
    this.emitStatusUpdated(meta);
  }

  override async setSwarmMode(input: SetSessionSwarmModeRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    // The engine closes a `task` / `tool` swarm itself at turn end and says so
    // with an `agent.status.updated` carrying `swarmMode: false`. Recording the
    // trigger is what makes the mode survive the next handle rebuild for a
    // manual swarm and, just as importantly, stop resurrecting an auto-exited
    // one — a stale `swarmMode: true` here would re-enter it on every rebuild.
    const previousTrigger = meta.swarmModeTrigger;
    meta.swarmMode = input.enabled;
    if (input.enabled) {
      meta.swarmModeTrigger = input.trigger;
    }
    try {
      await this.rebuildHandle(meta);
    } catch (error) {
      meta.swarmMode = !input.enabled;
      meta.swarmModeTrigger = previousTrigger;
      throw error;
    }
    this.emitStatusUpdated(meta);
  }

  override async undoHistory(input: SessionIdRpcInput & { count: number }): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    if (!meta.handle) {
      throw new KimiError(
        ErrorCodes.SESSION_NOT_FOUND,
        `cannot undo history of session "${input.sessionId}" without a live engine handle`,
      );
    }
    if (!Number.isSafeInteger(input.count) || input.count <= 0) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        'Undo count must be a positive safe integer',
        { details: { field: 'count' } },
      );
    }
    const history = await meta.handle.getHistory();
    const cut = computeUndoCut(history, input.count);
    if (cut.cutIndex < 0 || cut.removedCount < input.count) {
      throw new KimiError(
        ErrorCodes.SESSION_UNDO_UNAVAILABLE,
        formatUndoUnavailable(cut, input.count),
        {
          details: {
            reason: cut.stoppedAtCompaction
              ? 'compaction_boundary'
              : cut.removedCount === 0
                ? 'empty'
                : 'insufficient',
            requestedCount: input.count,
            undoableCount: cut.removedCount,
            stoppedAtCompaction: cut.stoppedAtCompaction,
          },
        },
      );
    }
    // Exclusive window so no turn is admitted mid-truncation. Without it the
    // cut raced an active or queued turn and spliced the history under it.
    const acquired = await meta.handle.tryAcquireQuiescence();
    if (!acquired) {
      throw new KimiError(
        ErrorCodes.SESSION_BUSY,
        'Cannot undo while a turn is active or queued. Wait for it to finish, then retry.',
        { details: { reason: 'loop' } },
      );
    }
    try {
      await meta.handle.setHistory(history.slice(0, cut.cutIndex));
      // Engine state follows the transcript: the engine pushes one checkpoint
      // at each undo-anchor turn's head, so unwinding the same number of
      // snapshots restores todo/plan/goal to the state the earliest cut turn
      // started from. An exhausted stack (a fork copied history only) is
      // normal and stops early without failing the undo.
      const { nativeRollbackEngineState } = await import('@moonshot-ai/kimi-agent/native');
      nativeRollbackEngineState(meta.workDir, meta.id, cut.removedCount);
    } finally {
      await meta.handle.releaseQuiescence();
    }
    meta.messageCount = Math.max(0, meta.messageCount - (history.length - cut.cutIndex));
    meta.contextTokens = 0;
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    await this.persistHistory(meta);
  }

  override async forkSession(input: ForkSessionInput): Promise<SessionSummary> {
    const source = this.requireSession(input.id);
    if (source.busy) {
      throw new KimiError(
        ErrorCodes.SESSION_FORK_ACTIVE_TURN,
        `cannot fork session "${input.id}" while a turn is active`,
      );
    }
    // A failed read is NOT an empty history: swallowing it created the fork
    // with no context and returned it as a success, and with a turnIndex the
    // failure surfaced as a bogus "Fork turn index is out of range" instead.
    // The read happens before the fork exists, so a throw leaves none behind.
    const history = source.handle ? await source.handle.getHistory() : [];
    if (input.turnIndex !== undefined) {
      // v1's fork rules, both bounds: the index must name a recorded user
      // turn. Anything outside [0, availableTurns) rejects with
      // request.invalid and leaves no fork behind. The lower bound is not
      // cosmetic — `retainThroughTurn` never matches `seen === turnIndex` for
      // a negative index and falls through to `history.length`, which would
      // silently retain the whole session under a "truncate at this turn"
      // call. v2 gets this from the engine, which owns the rule there.
      const availableTurns = history.filter(isUndoAnchorPrompt).length;
      if (input.turnIndex < 0 || input.turnIndex >= availableTurns) {
        throw new KimiError(ErrorCodes.REQUEST_INVALID, 'Fork turn index is out of range', {
          details: { turnIndex: input.turnIndex, availableTurns },
        });
      }
    }
    const forkId = input.forkId ?? `session_${randomUUID()}`;
    await this.createSession({ id: forkId, workDir: source.workDir });
    const forkMeta = this.requireSession(forkId);
    if (forkMeta.handle && history.length > 0) {
      const retained =
        input.turnIndex === undefined
          ? history
          : history.slice(0, retainThroughTurn(history, input.turnIndex));
      await forkMeta.handle.setHistory(retained);
      forkMeta.messageCount = retained.length;
    }
    if (input.title) {
      forkMeta.title = input.title;
      forkMeta.isCustomTitle = true;
      forkMeta.titleKind = 'custom';
    } else {
      // Upstream #3974: a fork without an explicit title takes the
      // `Fork: …` default and inherits the source's titleKind, so a
      // custom-titled source's fork keeps its title instead of being
      // overwritten by the auto-title generator (the old forced
      // `replaceable` let the first prompt replace it).
      forkMeta.title = `Fork: ${source.title || source.id}`;
      forkMeta.isCustomTitle = source.isCustomTitle;
      forkMeta.titleKind = source.titleKind;
    }
    const inheritedCustom = { ...source.custom };
    delete inheritedCustom['goal'];
    forkMeta.custom = { ...inheritedCustom, ...input.metadata };
    delete forkMeta.custom['goal'];
    // The fork inherits the source's runtime binding and plan document but
    // never its goal state (v1 dropped goal state at fork).
    forkMeta.model = source.model;
    forkMeta.thinkingEffort = source.thinkingEffort;
    forkMeta.permissionMode = source.permissionMode;
    forkMeta.planMode = source.planMode;
    if (source.plan !== undefined) {
      const forkPlanPath = posixPath(
        join(forkMeta.sessionDir, 'agents', 'main', 'plans', `${source.plan.id}.md`),
      );
      let content = source.plan.content;
      try {
        content = readFileSync(source.plan.path, 'utf-8');
      } catch {
        // no source plan file — carry the in-memory content
      }
      try {
        mkdirSync(dirname(forkPlanPath), { recursive: true });
        writeFileSync(forkPlanPath, content, 'utf8');
      } catch {
        // best-effort: the fork still reports the copied document
      }
      forkMeta.plan = { id: source.plan.id, content, path: forkPlanPath };
    }
    forkMeta.forkedFrom = source.id;
    forkMeta.updatedAt = Date.now();
    this.persistMeta(forkMeta);
    await this.persistHistory(forkMeta);
    return {
      id: forkId,
      workDir: forkMeta.workDir,
      sessionDir: forkMeta.sessionDir,
      title: forkMeta.title,
      createdAt: forkMeta.createdAt,
      updatedAt: forkMeta.updatedAt,
      metadata: input.metadata,
    };
  }

  override async exportSession(input: ExportSessionInput): Promise<ExportSessionResult> {
    // A one-shot CLI process holds no live session: resolve it from disk
    // with the same `session-meta.json` probe `listSessions` enumerates, so
    // `kimi export <id>` works for any persisted session, not only one this
    // process created. Everything below derives from the directory, so a
    // disk-resolved session exports identically to a live one.
    //
    // The id becomes a path segment twice (the session directory and the
    // default zip name), so it is checked before any join: an id carrying a
    // separator, a `..` segment, or an absolute root would otherwise read
    // and zip a directory outside the sessions root.
    assertSessionIdSegment(input.id);
    const live = this.liveSessions.get(input.id);
    const persisted =
      live === undefined ? this.loadMeta(join(this.sessionBaseDir, input.id)) : undefined;
    if (live === undefined && persisted === undefined) {
      throw new KimiError(ErrorCodes.SESSION_NOT_FOUND, `unknown session "${input.id}"`, {
        details: { sessionId: input.id },
      });
    }
    const sessionDir = live?.sessionDir ?? posixPath(join(this.sessionBaseDir, input.id));
    const zipPath = posixPath(
      input.outputPath ? resolve(input.outputPath) : join(this.sessionBaseDir, `${input.id}.zip`),
    );
    mkdirSync(dirname(zipPath), { recursive: true });

    // The global log (and its rotations) rides along only on request: it is
    // home-scoped, not session-scoped, so a session export stays focused by
    // default. The manifest names the primary entry when it is included.
    const globalLogDir = join(this.homeDir, 'logs');
    const globalLogEntries: string[] = [];
    if (input.includeGlobalLog === true && existsSync(globalLogDir)) {
      for (const file of readdirSync(globalLogDir).toSorted()) {
        if (file.startsWith('kimi-code.log')) {
          globalLogEntries.push(`logs/${file}`);
        }
      }
    }
    const globalLogPath = globalLogEntries.includes('logs/kimi-code.log')
      ? 'logs/kimi-code.log'
      : globalLogEntries[0];

    const manifest = {
      exportedAt: new Date().toISOString(),
      sessionId: input.id,
      kimiCodeVersion: input.version,
      wireProtocolVersion: '2.0.0',
      os: process.platform,
      nodejsVersion: process.version,
      ...(globalLogPath === undefined ? {} : { globalLogPath }),
    };

    const zipFile = new ZipFile();
    const entries: string[] = ['export-manifest.json'];
    zipFile.addBuffer(
      Buffer.from(JSON.stringify(manifest, null, 2), 'utf8'),
      'export-manifest.json',
    );

    if (existsSync(sessionDir)) {
      // Walk the session tree so nested artifacts (agents/, subagents/, …)
      // export under their relative posix paths like the engine's exporter.
      const walk = (dir: string, prefix: string): void => {
        let files;
        try {
          files = readdirSync(dir, { withFileTypes: true });
        } catch {
          return;
        }
        for (const file of files) {
          const relative = prefix === '' ? file.name : `${prefix}/${file.name}`;
          if (file.isDirectory()) {
            walk(join(dir, file.name), relative);
          } else if (file.isFile() && relative !== 'export-manifest.json') {
            zipFile.addFile(join(dir, file.name), relative);
            entries.push(relative);
          }
        }
      };
      walk(sessionDir, '');
    }

    for (const relative of globalLogEntries) {
      zipFile.addFile(join(this.homeDir, relative), relative);
      entries.push(relative);
    }

    await new Promise<void>((res, rej) => {
      const out = createWriteStream(zipPath);
      out.on('close', res);
      out.on('error', rej);
      zipFile.outputStream.pipe(out);
      zipFile.end();
    });

    return {
      zipPath,
      entries,
      sessionDir,
      manifest,
    };
  }

  override async getConfig(_input?: GetConfigOptions): Promise<KimiConfig> {
    // The runtime view is a lenient load: schema-invalid entries stay in the
    // document (validation defers to model resolution), so a broken alias
    // degrades the harness instead of blocking startup. `reload` re-reads the
    // file, which this loader already does on every call. Like v2, the
    // effective view has no v1-style `raw` passthrough — the raw document
    // lives in the file itself.
    const { raw: _raw, ...config } = loadRuntimeConfigLenient(this.configPath);
    void _raw;
    return config;
  }

  override async setConfig(patch: KimiConfigPatch): Promise<KimiConfig> {
    const current = readConfigFile(this.configPath);
    // Deep-merge per domain (v2 semantics): the previous top-level shallow
    // spread clobbered whole sections — a `{ models: { oneAlias } }` patch wiped
    // every other alias, `{ thinking: { enabled } }` dropped effort/budget, a
    // single provider edit lost its baseUrl/customHeaders, and any key present
    // but undefined in the patch cleared the stored value.
    const merged: Record<string, unknown> = {
      ...(current as Record<string, unknown>),
      raw: current.raw ? cloneRecord(current.raw) : undefined,
    };
    for (const [domain, domainPatch] of Object.entries(patch)) {
      if (domainPatch === undefined) continue;
      merged[domain] = deepMergeConfigValue(
        (current as Record<string, unknown>)[domain],
        domainPatch,
      );
    }
    const updated = validateConfig(merged);
    await writeConfigFile(this.configPath, updated);
    return updated;
  }

  override async removeProvider(providerId: string): Promise<KimiConfig> {
    // v1/v2 removal cascades: drop the provider entry, every model alias that
    // points at it, and any default pointer left dangling — persisted as ONE
    // atomic write so a crash can never leave a half-removed provider.
    const current = loadRuntimeConfig(this.configPath) as unknown as Record<string, unknown>;
    const providers = { ...(current['providers'] as Record<string, unknown> | undefined) };
    delete providers[providerId];
    const models = { ...(current['models'] as Record<string, unknown> | undefined) };
    for (const [alias, model] of Object.entries(models)) {
      if (isPlainObject(model) && model['provider'] === providerId) {
        delete models[alias];
      }
    }
    const next: Record<string, unknown> = { ...current, providers, models };
    if (current['defaultProvider'] === providerId) {
      delete next['defaultProvider'];
    }
    const defaultModel = current['defaultModel'];
    if (typeof defaultModel === 'string') {
      const alias = (current['models'] as Record<string, { provider?: string }> | undefined)?.[
        defaultModel
      ];
      if (alias?.provider === providerId) {
        delete next['defaultModel'];
      }
    }
    const agent = current['agent'];
    if (isPlainObject(agent) && agent['nativeLlmProvider'] === providerId) {
      const nextAgent = { ...agent };
      delete nextAgent['nativeLlmProvider'];
      next['agent'] = nextAgent;
    }
    const updated = validateConfig(next);
    await writeConfigFile(this.configPath, updated);
    return updated;
  }

  override async importCustomRegistry(
    options: ImportCustomRegistryOptions,
  ): Promise<ImportCustomRegistryResult> {
    const { url } = options;
    const current = loadRuntimeConfig(this.configPath) as unknown as Record<string, unknown>;
    const providers = (current['providers'] ?? {}) as Record<string, unknown>;
    // A re-import commonly omits the key: the registry URL is the stable
    // identity, so fall back to the key a previous import from the same URL
    // parked on its providers.
    const source: CustomRegistrySource = {
      kind: 'apiJson',
      url,
      apiKey: options.apiKey ?? registryKeyFromExisting(providers, url) ?? '',
    };

    let entries: Record<string, CustomRegistryProviderEntry>;
    try {
      entries = await fetchCustomRegistry(source, { userAgent: this.outboundUserAgent() });
    } catch (error) {
      throw new RegistryImportError(
        `custom registry at ${url} cannot be imported: ${truncateUpstreamMessage(error)}`,
        'fetch',
        error instanceof CustomRegistryApiError ? error.status : undefined,
      );
    }
    const entryList = Object.values(entries);
    if (entryList.length === 0) {
      throw new RegistryImportError(
        `custom registry at ${url} has no importable providers`,
        'empty',
      );
    }
    for (const entry of entryList) {
      const existing = providers[entry.id];
      if (isPlainObject(existing) && existing['oauth'] !== undefined) {
        throw new RegistryImportError(
          `provider ${entry.id} is managed by OAuth login; log out before importing it`,
          'apply',
        );
      }
    }

    const previousDefault = current['defaultModel'];
    const previousDefaultProvider = current['defaultProvider'];
    const previousThinking = current['thinking'];
    const next = {
      providers: { ...providers },
      models: { ...((current['models'] ?? {}) as Record<string, unknown>) },
    } as unknown as ManagedKimiConfigShape;
    next.defaultModel = typeof previousDefault === 'string' ? previousDefault : undefined;
    next['defaultProvider'] =
      typeof previousDefaultProvider === 'string' ? previousDefaultProvider : undefined;
    next.thinking = previousThinking as ManagedKimiConfigShape['thinking'];

    try {
      applyCustomRegistryEntries(next, entries, source);
    } catch (error) {
      throw new RegistryImportError(
        `custom registry at ${url} cannot be imported: ${truncateUpstreamMessage(error)}`,
        'apply',
      );
    }

    const firstEntry = entryList[0];
    const firstModelKey = firstEntry === undefined ? undefined : Object.keys(firstEntry.models)[0];
    const hadDefault = typeof previousDefault === 'string' && previousDefault.trim().length > 0;
    if (
      options.setDefaultWhenUnset !== false &&
      !hadDefault &&
      firstEntry !== undefined &&
      firstModelKey !== undefined
    ) {
      next.defaultModel = `${firstEntry.id}/${firstModelKey}`;
    }

    const merged: Record<string, unknown> = {
      ...current,
      providers: next.providers,
      models: next.models,
    };
    assignOrDelete(merged, 'defaultModel', next.defaultModel);
    assignOrDelete(merged, 'defaultProvider', next['defaultProvider']);
    assignOrDelete(merged, 'thinking', next.thinking);
    const updated = validateConfig(merged);
    await writeConfigFile(this.configPath, updated);

    const hasCredential = source.apiKey.length > 0;
    return {
      providers: entryList.map((entry) => ({
        id: entry.id,
        type: entry.type,
        base_url: entry.api,
        has_api_key: hasCredential,
        status: hasCredential ? 'connected' : 'unconfigured',
        models: Object.keys(entry.models),
      })),
      modelsImported: entryList.reduce(
        (total, entry) => total + Object.keys(entry.models).length,
        0,
      ),
      credentialEnv: credentialEnvHints(entryList),
    };
  }

  private outboundUserAgent(): string | undefined {
    if (!this.identity) return undefined;
    return createKimiDefaultHeaders({ homeDir: this.homeDir, ...this.identity })['User-Agent'];
  }

  override supportsAtomicSectionReplace(): boolean {
    return true;
  }

  override async replaceConfigSections(sections: Record<string, unknown>): Promise<void> {
    // Atomic multi-section replace: `undefined` clears a section, any other
    // value replaces it wholesale (no merge). Written in one pass so a crash
    // cannot leave the file half-updated.
    const current = loadRuntimeConfig(this.configPath) as unknown as Record<string, unknown>;
    const next: Record<string, unknown> = { ...current };
    for (const [section, value] of Object.entries(sections)) {
      if (value === undefined) {
        delete next[section];
      } else {
        next[section] = value;
      }
    }
    const updated = validateConfig(next);
    await writeConfigFile(this.configPath, updated);
  }

  override async getConfigDiagnostics(): Promise<ConfigDiagnostics> {
    return { warnings: [] };
  }

  override async uploadFile(data: Uint8Array, options: UploadFileOptions): Promise<FileMeta> {
    const filesDir = join(this.homeDir, 'files');
    mkdirSync(filesDir, { recursive: true });
    const fileId = `file_${randomUUID()}`;
    const filePath = join(filesDir, `${fileId}_${options.name}`);
    writeFileSync(filePath, data);
    return {
      id: fileId,
      name: options.name,
      media_type: options.mimeType ?? 'application/octet-stream',
      size: data.byteLength,
      created_at: new Date().toISOString(),
      expires_at: options.expiresInSec
        ? new Date(Date.now() + options.expiresInSec * 1000).toISOString()
        : undefined,
    };
  }

  override async deleteFile(fileId: string): Promise<void> {
    const filesDir = join(this.homeDir, 'files');
    if (!existsSync(filesDir)) return;
    try {
      const names = readdirSync(filesDir);
      for (const name of names) {
        if (name.startsWith(`${fileId}_`)) {
          try {
            unlinkSync(join(filesDir, name));
          } catch {
            // ignore
          }
        }
      }
    } catch {
      // ignore
    }
  }

  override async reloadSession(input: ReloadSessionRpcInput): Promise<ResumedSessionSummary> {
    return this.resumeSession({ id: input.sessionId });
  }

  override async getSessionWarnings(
    input: SessionIdRpcInput,
  ): Promise<readonly { code: string; message: string; severity: 'warning' }[]> {
    const meta = this.requireSession(input.sessionId);
    if (meta.handle === undefined) return [];
    // The engine's own degradations — MCP servers it could not connect, and
    // servers waiting on the user's authorization. This used to answer `[]`
    // unconditionally, so a user whose MCP tools were missing got no reason.
    const warnings = await meta.handle.warnings();
    return warnings as readonly { code: string; message: string; severity: 'warning' }[];
  }

  override async getTodos(input: SessionIdRpcInput): Promise<readonly SessionTodoItem[]> {
    const meta = this.requireSession(input.sessionId);
    // The engine owns the todo state, and it lives under the workspace's
    // engine-state directory — whose name is a digest of the canonicalized
    // workspace path, so the host asks the engine for it rather than guessing.
    // The previous guess read `<sessionDir>/todo.json`, which nothing ever
    // writes, so this always answered `[]` and `/undo` could not restore the
    // panel it had just cleared.
    const { nativeReadEngineState } = await import('@moonshot-ai/kimi-agent/native');
    const raw = nativeReadEngineState(meta.workDir, 'todo', meta.id);
    if (raw === null) return [];
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return [];
    }
    if (!Array.isArray(parsed)) return [];
    return parsed.map((entry: { title?: string; status?: string }) => ({
      title: String(entry.title ?? ''),
      status:
        entry.status === 'done' || entry.status === 'completed'
          ? 'done'
          : entry.status === 'in_progress'
            ? 'in_progress'
            : 'pending',
    }));
  }

  override async cancelShellCommand(input: {
    sessionId: string;
    commandId: string;
  }): Promise<void> {
    this.requireSession(input.sessionId);
    const key = shellCommandKey(input.sessionId, input.commandId);
    const id = key === undefined ? undefined : this.liveShellCommands.get(key);
    // Nothing to kill means the command already finished (or never started) —
    // not an error, and not a claim that something was cancelled.
    if (id === undefined) return;
    const { nativeBashKill } = await import('@moonshot-ai/kimi-agent/native');
    nativeBashKill(id);
  }

  override async swarm(input: SessionPromptRpcInput): Promise<void> {
    await this.setSwarmMode({ sessionId: input.sessionId, enabled: true, trigger: 'task' });
    return this.prompt(input);
  }

  override async compact(input: SessionIdRpcInput & CompactOptions): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    const handle = meta.handle;
    if (!handle) {
      throw new KimiError(
        ErrorCodes.COMPACTION_UNABLE,
        'No engine handle available for compaction',
      );
    }
    const historyLen = await handle.historyLen();
    if (historyLen === 0) {
      throw new KimiError(ErrorCodes.COMPACTION_UNABLE, 'No messages to compact');
    }
    // Stricter than v2 on purpose: `agent-core-v2` guarded only the empty case
    // (`fullCompactionService.ts` → "No messages to compact in current
    // history.", pinned by its `rejects manual compaction ... when history is
    // empty`), so a single-message session would summarize one message into
    // one message and spend a model call to do it. One message is not a
    // conversation, so refuse instead.
    if (historyLen <= 1) {
      throw new KimiError(ErrorCodes.COMPACTION_UNABLE, 'History too short to compact');
    }
    const acquired = await handle.tryAcquireQuiescence();
    if (!acquired) {
      throw new KimiError(ErrorCodes.COMPACTION_FAILED, 'Session is busy');
    }
    this.compactionCancels.delete(meta.id);
    this.receiveEvent({
      type: 'compaction.started',
      sessionId: meta.id,
      agentId: 'main',
      trigger: 'manual',
      instruction: input.instruction,
    });
    try {
      // Engine-side: the session's own model writes the summary of the deep
      // prefix (honoring `instruction`) and the smallest safe tail stays
      // verbatim — no host-side placeholder message.
      const report = await handle.compact(input.instruction);
      if (!report.changed) {
        throw new KimiError(ErrorCodes.COMPACTION_UNABLE, 'History too short to compact');
      }
      meta.messageCount = report.messageCount;
      this.persistMeta(meta);
      await this.persistHistory(meta);
      this.receiveEvent({
        type: 'compaction.completed',
        sessionId: meta.id,
        agentId: 'main',
        result: {
          summary: report.summary ?? '',
          compactedCount: report.compactedCount ?? 0,
          tokensBefore: report.tokensBefore,
          tokensAfter: report.tokensAfter,
        },
      });
    } catch (error) {
      // The transcript's compaction block only closes on a terminal event, so
      // every failure after `started` must emit one — a user-triggered cancel
      // resolves silently, any other failure still throws with the real reason.
      const cancelled = this.compactionCancels.delete(meta.id);
      this.receiveEvent({
        type: 'compaction.cancelled',
        sessionId: meta.id,
        agentId: 'main',
      });
      if (cancelled) return;
      if (error instanceof KimiError) throw error;
      throw new KimiError(
        ErrorCodes.COMPACTION_FAILED,
        error instanceof Error ? error.message : String(error),
      );
    } finally {
      await handle.releaseQuiescence();
    }
  }

  override async cancelCompaction(input: SessionIdRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    // True only when the engine had a compaction in flight; remembering it
    // lets `compact` translate the engine's cancellation error into a
    // `compaction.cancelled` event instead of a failure.
    if ((await meta.handle?.cancelCompaction()) === true) {
      this.compactionCancels.add(meta.id);
    }
  }

  override async generateAgentsMd(input: SessionIdRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    if (!meta.model) {
      throw new KimiError(ErrorCodes.SESSION_INIT_FAILED, 'Main agent has no model bound');
    }
    if (!meta.handle) {
      throw new KimiError(ErrorCodes.SESSION_INIT_FAILED, 'Main agent has no live engine handle');
    }
    // The /init run is a session-level operation pinned to the main agent: it
    // launches its own turn (a subagent system trigger on the engine), does
    // not touch the prompt-derived metadata, and awaits the run so a failed
    // launch surfaces as session.init_failed.
    const turnId = await meta.handle.enqueueTurn(
      { role: 'user', content: DEFAULT_INIT_PROMPT },
      'newTurn',
    );
    try {
      await meta.handle.turnOutcome(turnId);
    } catch (error) {
      throw new KimiError(ErrorCodes.SESSION_INIT_FAILED, 'Session init failed', {
        cause: error,
      });
    } finally {
      meta.busy = false;
      await this.persistHistory(meta);
    }
  }

  override async promptWithSkills(input: SessionPromptWithSkillsRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    // v2's two pre-checks (`skillService.ts:126-134`), before the catalog is
    // touched. The empty-input half is defence at this seam: the public
    // `Session.promptWithSkills` rejects an empty prompt earlier, in the same
    // normalize step `prompt()` uses, so this only fires for a direct RPC
    // caller. The empty-skills half is the one that bites in practice — without
    // it the submission degraded into an ordinary prompt that activated nothing.
    if (input.input.length === 0) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        'promptWithSkills requires a non-empty prompt',
      );
    }
    if (input.skills.length === 0) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        'promptWithSkills requires at least one skill',
      );
    }
    const wireMetadata = originClientMetadata(input.clientMetadata);
    // The bundle is validated as a unit before any turn is queued, matching
    // `activateSkill`'s failure mode. Dropping an unknown name instead would
    // hand the caller a successful turn that activated less than it asked
    // for, and the model would see a prompt missing the skill the user named.
    const activations: BundledSkillActivation[] = [];
    const renderedBlocks: { type: 'text'; text: string }[] = [];
    for (const skill of input.skills) {
      // Trim before the lookup, the way `activateSkill` does: an untrimmed
      // name would miss the skill and fail the whole bundle.
      const name = skill.name.trim();
      const args = skill.args?.trim();
      // Throws `skill.not_found` / `skill.type_unsupported` exactly as
      // `activateSkill` does, so one unknown or non-activatable name rejects
      // the bundle (v2 `prepareBundled`, `skillService.ts:189-204`).
      const resolved = await this.resolveSkillPrompt(meta, name, args);
      activations.push({
        activationId: `skill_${randomUUID()}`,
        skillName: resolved.name,
        ...(args !== undefined && args.length > 0 ? { skillArgs: args } : {}),
        ...(resolved.skillType !== undefined ? { skillType: resolved.skillType } : {}),
        ...(resolved.path !== undefined ? { skillPath: resolved.path } : {}),
        ...(resolved.source !== undefined ? { skillSource: resolved.source } : {}),
      });
      renderedBlocks.push({ type: 'text', text: resolved.text });
    }
    // v2 `AgentSkillService.promptWithSkills` (`skillService.ts:139-146`) derives
    // the prompt metadata from **the caller's own parts** — `input.input`, before
    // the rendered skill blocks are prepended — and does it before recording the
    // activations. `prompt()` cannot supply that here: it derives from whatever
    // parts it is handed, and the content this turn needs is
    // `[...renderedBlocks, ...input.input]` (v2 `:155`), so handing both to
    // `prompt()` made the title/lastPrompt the skill's rendered body instead of
    // the user's sentence. Applied here with the caller's parts, exactly as
    // `activateSkill` applies `/name args` over its rendered prompt.
    //
    // v2 gates on the main agent (`agentContext.agentId === MAIN_AGENT_ID`): a
    // btw side-channel turn leaves the session metadata untouched, which is what
    // `prompt()`'s own non-main early return already did before this call.
    if (this.interactiveAgentId === 'main') {
      this.applyPromptMetadata(
        meta,
        promptMetadataTextFromPrompt(input.input),
        input.clientMetadata?.displayText,
      );
    }
    // Each activation is published before the turn, the same ordering
    // `activateSkill` uses, so the live transcript shows the cards ahead of
    // `turn.started` and `message-dispatch` can group them with the prompt
    // they were bundled into.
    //
    // `agentId` is the interactive-agent scope, not a constant: v2 stamps
    // `SkillActivated` with `scopeContext.agentContext.agentId`
    // (`skillService.ts:248`), so a btw side-channel activation belongs to the
    // subagent that ran it. Hardcoding `main` attributed every bundled
    // activation to the main transcript — including the btw panel's, which has
    // no skill-card surface of its own yet (the event is honest now; that
    // rendering gap is a separate item).
    for (const activation of activations) {
      this.trackSkillInvocation(activation.skillName, 'user-slash', activation.skillType);
      this.receiveEvent({
        sessionId: meta.id,
        agentId: this.interactiveAgentId,
        type: 'skill.activated',
        activationId: activation.activationId,
        skillName: activation.skillName,
        ...(activation.skillArgs !== undefined ? { skillArgs: activation.skillArgs } : {}),
        trigger: 'user-slash',
        ...(activation.skillPath !== undefined ? { skillPath: activation.skillPath } : {}),
        ...(activation.skillSource !== undefined ? { skillSource: activation.skillSource } : {}),
      });
    }
    return this.prompt({
      sessionId: meta.id,
      // protocol `UserPromptOrigin.skillActivations` and v2 `skillService.ts:155`:
      // the rendered blocks precede the caller's parts, and the same list rides
      // the turn so resume / replay rebuilds the per-skill view from the one
      // message. The metadata step above already ran, from the caller's parts.
      input: [...renderedBlocks, ...input.input],
      clientMetadata: {
        ...input.clientMetadata,
        // v2's bundle origin carries the metadata entries and the activation
        // list (`skillService.ts:158-163`); the entries were dropped here, so a
        // bundled prompt's displayText never reached the transcript fold.
        origin: {
          kind: 'user',
          skillActivations: activations,
          ...(wireMetadata !== undefined ? { clientMetadata: wireMetadata } : {}),
          ...(input.attachments !== undefined && input.attachments.length > 0
            ? { attachments: input.attachments }
            : {}),
        },
      },
      skipPromptMetadata: true,
    });
  }

  override async runShellCommand(input: {
    sessionId: string;
    command: string;
    commandId?: string;
  }): Promise<{ stdout: string; stderr: string; isError?: boolean; backgrounded?: boolean }> {
    const meta = this.requireSession(input.sessionId);
    // The TUI routes live `!` command output and ctrl+b detach by these events;
    // without them the live view stayed empty until the command finished.
    const commandId = input.commandId ?? randomUUID();
    try {
      const { nativeBashSpawn, nativeBashWait } = await import('@moonshot-ai/kimi-agent/native');
      const shell = probeShellPath() ?? 'bash';
      let stdout = '';
      let stderr = '';
      let taskId: string | undefined;
      const { id } = nativeBashSpawn(
        {
          argv: [shell, '-c', input.command],
          cwd: meta.workDir,
        },
        (_err, ev) => {
          if (!ev) return;
          if (ev.kind === 'stdout' && ev.data) {
            stdout += ev.data;
            this.receiveEvent({
              sessionId: input.sessionId,
              agentId: 'main',
              type: 'shell.output',
              commandId,
              update: { kind: 'stdout', text: ev.data },
              ...(taskId !== undefined ? { taskId } : {}),
            });
          }
          if (ev.kind === 'stderr' && ev.data) {
            stderr += ev.data;
            this.receiveEvent({
              sessionId: input.sessionId,
              agentId: 'main',
              type: 'shell.output',
              commandId,
              update: { kind: 'stderr', text: ev.data },
              ...(taskId !== undefined ? { taskId } : {}),
            });
          }
        },
      );
      taskId = String(id);
      this.receiveEvent({
        sessionId: input.sessionId,
        agentId: 'main',
        type: 'shell.started',
        commandId,
        taskId: String(id),
      });
      // Publish the handle so `cancelShellCommand` can reach the process this
      // call spawned. Without it the cancel had nothing to kill and reported
      // success while the command kept running.
      const key = shellCommandKey(input.sessionId, input.commandId);
      if (key !== undefined) this.liveShellCommands.set(key, id);
      try {
        const exit = await nativeBashWait(id);
        const isError = exit.exitCode !== 0 || Boolean(exit.error);
        this.receiveEvent({
          sessionId: input.sessionId,
          agentId: 'main',
          type: 'shell.completed',
          commandId,
          isError,
          taskId: String(id),
        });
        return {
          stdout,
          stderr,
          isError,
          backgrounded: false,
        };
      } finally {
        if (key !== undefined) this.liveShellCommands.delete(key);
      }
    } catch {
      return new Promise((res) => {
        exec(input.command, { cwd: meta.workDir }, (error, stdout, stderr) => {
          res({
            stdout: stdout ?? '',
            stderr: stderr ?? '',
            isError: !!error,
            backgrounded: false,
          });
        });
      });
    }
  }

  override async getContext(input: SessionIdRpcInput): Promise<AgentContextData> {
    const meta = this.requireSession(input.sessionId);
    const history = meta.handle ? await meta.handle.getHistory() : [];
    return this.contextFromHistory(meta, history);
  }

  override async startBtw(input: SessionIdRpcInput): Promise<string> {
    const meta = this.requireSession(input.sessionId);
    if (!meta.handle) {
      throw new KimiError(
        ErrorCodes.SESSION_NOT_FOUND,
        `cannot start a btw side channel on unknown or closed session "${input.sessionId}"`,
      );
    }
    const agentId = await meta.handle.startBtw();
    meta.agents = meta.agents ?? {
      main: { homedir: meta.workDir, type: 'main', parentAgentId: null },
    };
    meta.agents[agentId] = {
      homedir: meta.workDir,
      type: 'sub',
      parentAgentId: 'main',
    };
    this.persistMeta(meta);
    return agentId;
  }

  /**
   * Title generation over the live engine history: the engine applies the
   * first_turn / user_prompts rule to the session's cross-turn history, or
   * asks the managed platform through the `chat_title` tool when `source` is
   * `digest`. A generated title lands as `titleKind: 'generated'`; without
   * `force` an existing generated or host-custom title is returned as-is.
   */
  override async generateSessionTitle(
    input: GenerateSessionTitleInput,
  ): Promise<string | undefined> {
    const meta = this.requireSession(input.id);
    if (!meta.handle) {
      throw new KimiError(
        ErrorCodes.SESSION_NOT_FOUND,
        `cannot generate a title for unknown or closed session "${input.id}"`,
      );
    }
    if (
      input.source !== undefined &&
      input.source !== 'first_turn' &&
      input.source !== 'user_prompts' &&
      input.source !== 'digest'
    ) {
      // The three checks above exhaust the declared union, so this branch is
      // only reachable from an untyped caller; `String` keeps the message
      // honest without widening the parameter.
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `unsupported title source "${String(input.source)}": expected 'first_turn', 'user_prompts' or 'digest'`,
      );
    }
    if (!input.force && (meta.titleKind === 'generated' || meta.titleKind === 'custom')) {
      return meta.title;
    }
    const title = await meta.handle.generateTitle(input.source);
    if (title === null) return undefined;
    meta.title = title;
    meta.titleKind = 'generated';
    meta.isCustomTitle = false;
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    return title;
  }

  override async getCronTasks(input: SessionIdRpcInput): Promise<GetCronTasksResult> {
    const meta = this.requireSession(input.sessionId);
    // The cron registry is workspace-level engine state (the state-bridge
    // `cron` domain carries no session id), so the host reads it the same way
    // it reads the todo domain: through the engine, which owns the digest
    // layout of the workspace state directory.
    const { nativeCronNextFire, nativeReadEngineState } =
      await import('@moonshot-ai/kimi-agent/native');
    const raw = nativeReadEngineState(meta.workDir, 'cron', meta.id);
    if (raw === null) return { tasks: [] };
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return { tasks: [] };
    }
    if (!Array.isArray(parsed)) return { tasks: [] };
    const now = Date.now();
    const tasks = parsed.map((entry: Record<string, unknown>) => {
      const id = typeof entry['id'] === 'string' ? entry['id'] : '';
      const cron = typeof entry['cron'] === 'string' ? entry['cron'] : '';
      const createdAt =
        typeof entry['createdAt'] === 'number'
          ? entry['createdAt']
          : typeof entry['created_at'] === 'number'
            ? entry['created_at']
            : undefined;
      const lastFiredAt =
        typeof entry['lastFiredAt'] === 'number' ? entry['lastFiredAt'] : undefined;
      // The post-jitter next fire comes from the engine: the host cannot
      // reproduce the parser, the local timezone, or the jitter derivation.
      let nextFireAt: number | null = null;
      if (id !== '' && cron !== '') {
        try {
          nextFireAt = nativeCronNextFire(JSON.stringify(entry), now);
        } catch {
          nextFireAt = null;
        }
      }
      return {
        id,
        cron,
        recurring: entry['recurring'] !== false,
        createdAt: createdAt ?? 0,
        lastFiredAt,
        nextFireAt,
      };
    });
    return { tasks };
  }

  override async listWorkspaceSkills(workDir: string): Promise<readonly SkillSummary[]> {
    const root = normalizeRequiredWorkDir('listWorkspaceSkills', workDir);
    const skills: SkillSummary[] = [];
    const seen = new Set<string>();
    const dirsToScan = [
      join(root, '.agents', 'skills'),
      join(root, '.kimi-code', 'skills'),
      join(this.homeDir, 'skills'),
      ...this.skillDirs,
    ];
    for (const dir of dirsToScan) {
      const scanRoot = posixPath(dir);
      if (!existsSync(scanRoot)) continue;
      try {
        const entries = readdirSync(scanRoot, { withFileTypes: true });
        for (const entry of entries) {
          if (entry.isDirectory()) {
            const skillMd = posixPath(join(scanRoot, entry.name, 'SKILL.md'));
            if (existsSync(skillMd) && !seen.has(entry.name)) {
              seen.add(entry.name);
              skills.push({
                name: entry.name,
                ...this.readSkillSummary(skillMd),
                path: skillMd,
                source: scanRoot.startsWith(root) ? 'project' : 'user',
              });
            }
          }
        }
      } catch {
        // ignore
      }
    }
    return skills;
  }

  /** The frontmatter-derived skill summary fields (description, invocation gate). */
  private readSkillSummary(skillMd: string): {
    description: string;
    disableModelInvocation?: boolean;
  } {
    try {
      const content = readFileSync(skillMd, 'utf8');
      const frontmatter = content.match(/^---\n([\s\S]*?)\n---/);
      const meta = frontmatter?.[1] ?? '';
      const descriptionMatch = meta.match(/description:\s*(.+)/i);
      const disableMatch = meta.match(/disable_model_invocation:\s*(.+)/i);
      const disableModelInvocation =
        disableMatch?.[1] !== undefined ? disableMatch[1].trim() === 'true' : undefined;
      return {
        description:
          descriptionMatch?.[1]?.trim() || `Skill: ${posixPath(skillMd).split('/').at(-2)}`,
        ...(disableModelInvocation !== undefined ? { disableModelInvocation } : {}),
      };
    } catch {
      return { description: 'Skill: ' + posixPath(skillMd).split('/').at(-2) };
    }
  }

  override async listSkills(input: SessionIdRpcInput): Promise<readonly SkillSummary[]> {
    const meta = this.requireSession(input.sessionId);
    // v2 serves the skill catalog from the engine, and this list drives the
    // slash panel: the builtin product skills, the dotted sub-skill commands
    // and `extra_skill_dirs` exist only there — a host-side scan listed none of
    // them, so `/custom-theme` and `/sub-skill.review` never appeared even
    // though the system prompt advertised them.
    const engine = meta.handle ? await meta.handle.skills() : undefined;
    if (engine === undefined) {
      return this.listWorkspaceSkills(meta.workDir);
    }
    const merged: SkillSummary[] = [...(engine as SkillSummary[])];
    const seen = new Set(merged.map((skill) => skill.name.toLowerCase()));
    // Keep the host's own roots (it scans `<home>/skills`, which the engine
    // does not) and let the engine's precedence win on a name clash.
    for (const skill of await this.listWorkspaceSkills(meta.workDir)) {
      if (seen.has(skill.name.toLowerCase())) continue;
      seen.add(skill.name.toLowerCase());
      merged.push(skill);
    }
    return merged;
  }

  override async activateSkill(input: ActivateSkillRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    const name = input.name.trim();
    if (name.length === 0) {
      throw new KimiError(ErrorCodes.SKILL_NAME_EMPTY, 'Skill name cannot be empty');
    }
    const args = input.args?.trim();
    const wireMetadata = originClientMetadata(input.clientMetadata);
    const resolved = await this.resolveSkillPrompt(meta, name, args);
    const rendered = resolved.text;
    const activationId = `skill_${randomUUID()}`;
    this.trackSkillInvocation(resolved.name, 'user-slash', resolved.skillType);
    // v1/v2 published the activation event before the turn launched, so the
    // event stream orders it ahead of turn.started. v2 also stamps the scope's
    // agent (`skillService.ts:248`) rather than a constant, and takes the
    // provenance from the catalog instead of guessing it from a path.
    this.receiveEvent({
      sessionId: meta.id,
      agentId: this.interactiveAgentId,
      type: 'skill.activated',
      activationId,
      skillName: resolved.name,
      ...(args !== undefined && args.length > 0 ? { skillArgs: args } : {}),
      trigger: 'user-slash',
      ...(resolved.path !== undefined ? { skillPath: resolved.path } : {}),
      ...(resolved.source !== undefined ? { skillSource: resolved.source } : {}),
    });
    // v2 #3832: the same activation rides the turn request as a
    // `skill_activation` prompt origin, so the engine echoes it on the turn
    // events and an origin-aware consumer can tell this turn from a typed
    // prompt. The web client nests the identical shape in its submission
    // metadata (`metadata.origin`).
    const origin: SkillActivationOrigin = {
      kind: 'skill_activation',
      activationId,
      skillName: resolved.name,
      ...(args !== undefined && args.length > 0 ? { skillArgs: args } : {}),
      trigger: 'user-slash',
      ...(resolved.skillType !== undefined ? { skillType: resolved.skillType } : {}),
      ...(resolved.path !== undefined ? { skillPath: resolved.path } : {}),
      ...(resolved.source !== undefined ? { skillSource: resolved.source } : {}),
      // v2's origin carries the metadata entries as an array
      // (`skillService.ts:98`); `undefined` when the client sent none.
      ...(wireMetadata !== undefined ? { clientMetadata: wireMetadata } : {}),
      ...(input.attachments !== undefined && input.attachments.length > 0
        ? { attachments: input.attachments }
        : {}),
    };
    const clientMetadata: ClientPromptMetadata = {
      ...input.clientMetadata,
      origin,
    };
    // v2 `activate` submits `[rendered, ...(input.content ?? [])]`
    // (`skillService.ts:73-85`): the skill block leads, the caller's own parts
    // follow in the same turn.
    const parts: PromptPart[] = [{ type: 'text', text: rendered }, ...(input.content ?? [])];
    // The activation updates the prompt-derived metadata like a prompt whose
    // text is the slash command itself — with this entry's displayText
    // winning over the raw slash text when the client supplied one (v2
    // #3764). Applied exactly once here, like upstream's single
    // `promptMetadataTextFromSkill` application: the busy path below
    // resubmits the rendered prompt as a steer with the metadata step
    // skipped, so the steered activation keeps this entry's metadata.
    this.applyPromptMetadata(
      meta,
      promptMetadataTextFromText(`/${name}${args ? ` ${args}` : ''}`),
      input.clientMetadata?.displayText,
    );
    // A turn already running steers the activation into it (v2 #3832: the
    // steered activation gets a transcript frame via the same event); a
    // fresh `newTurn` enqueue here would queue a second turn behind a busy
    // session instead of joining the running one.
    if (meta.busy) {
      return this.steer({
        sessionId: meta.id,
        input: parts,
        clientMetadata,
        skipPromptMetadata: true,
      });
    }
    return this.prompt({
      sessionId: meta.id,
      input: parts,
      clientMetadata,
      skipPromptMetadata: true,
    });
  }

  /**
   * Resolve a `/skill:` activation through the engine and render its prompt —
   * v2 `AgentSkillService` asks its own catalog for both
   * (`skillService.ts:207` + `prepareBundled`), and so does this: the engine
   * resolves the name against the whole scan (project, user, `extra_skill_dirs`,
   * builtins), applies the user-activatable type gate, and expands
   * `$ARGUMENTS` / `${KIMI_SKILL_DIR}`.
   *
   * Throws the two errors v2 raises, so a caller cannot tell the paths apart:
   * `skill.not_found` for an unknown name, `skill.type_unsupported` for a skill
   * whose `type` is not user-activatable (v2 `isUserActivatableSkillType`).
   */
  private async resolveSkillPrompt(
    meta: NativeSessionMeta,
    name: string,
    args: string | undefined,
  ): Promise<ResolvedSkillPrompt> {
    const rendered = await meta.handle?.renderSkillPrompt(name, args ?? '');
    if (rendered !== undefined && rendered.status !== 'ok') {
      if (rendered.status === 'type_unsupported') {
        const resolved = rendered.name ?? name;
        throw new KimiError(
          ErrorCodes.SKILL_TYPE_UNSUPPORTED,
          `Skill "${resolved}" cannot be activated by the user`,
        );
      }
      throw new KimiError(ErrorCodes.SKILL_NOT_FOUND, `Skill "${name}" was not found`);
    }
    if (rendered?.text !== undefined) {
      return {
        text: rendered.text,
        name: rendered.name ?? name,
        ...(rendered.path !== undefined ? { path: rendered.path } : {}),
        // The wire type is a closed union; the engine's own vocabulary is
        // `project` | `user` | `builtin`. A value outside it is dropped rather
        // than widened, so an origin never carries a source no consumer knows.
        ...(SKILL_SOURCES.has(rendered.source ?? '')
          ? { source: rendered.source as SkillSource }
          : {}),
        ...(rendered.skillType !== undefined ? { skillType: rendered.skillType } : {}),
      };
    }
    // An addon predating `sessionRenderSkillPrompt`. Project-scope only, no
    // expansion, no type gate — the degraded path the fork had before, kept so
    // an older binary still activates the skills it always did.
    const fallback = this.renderProjectSkillPrompt(meta, name, args);
    if (fallback === undefined) {
      throw new KimiError(ErrorCodes.SKILL_NOT_FOUND, `Skill "${name}" was not found`);
    }
    return fallback;
  }

  /**
   * One activation's telemetry (v2 `AgentSkillService.publishActivation`,
   * `skillService.ts:277-287`): `skill_invoked` for every activation, plus
   * `flow_invoked` when the skill declares `type: flow`.
   *
   * The model-tool path already emits these from the engine
   * (`tools/skill.rs`); the user-slash path records its activation on the host
   * side, so it emits them here — without this, only model-invoked skills were
   * counted at all.
   */
  private trackSkillInvocation(
    skillName: string,
    trigger: 'user-slash',
    skillType: string | undefined,
  ): void {
    this.telemetry.track('skill_invoked', { skill_name: skillName, trigger });
    if (skillType === 'flow') {
      this.telemetry.track('flow_invoked', { flow_name: skillName });
    }
  }

  /**
   * The pre-engine renderer, kept only as the fallback above: the instruction
   * line plus a `<skill-loaded>` wrapper over the body read from
   * `<workDir>/.kimi-code/skills/<name>/SKILL.md`. It finds project skills and
   * nothing else, and expands no arguments — which is why the engine owns this
   * now.
   */
  private renderProjectSkillPrompt(
    meta: NativeSessionMeta,
    name: string,
    args: string | undefined,
  ): ResolvedSkillPrompt | undefined {
    const skillDir = join(meta.workDir, '.kimi-code', 'skills', name);
    const skillMd = join(skillDir, 'SKILL.md');
    if (!existsSync(skillMd)) return undefined;
    let body: string;
    try {
      const raw = readFileSync(skillMd, 'utf8');
      // Strip the frontmatter: the model sees the body, the host the metadata.
      const withoutFrontmatter = raw.replace(/^---\n[\s\S]*?\n---\n/, '');
      body = withoutFrontmatter.trimEnd();
    } catch {
      return undefined;
    }
    const trimmedArgs = args?.trim();
    const lines = [
      `User activated the skill "${name}". Follow the loaded skill instructions.`,
      '',
      `<skill-loaded name="${name}" trigger="user-slash" source="project" dir="${posixPath(skillDir)}"${trimmedArgs ? ` args="${trimmedArgs}"` : ''}>`,
      body,
      '',
      ...(trimmedArgs ? ['ARGUMENTS: ' + trimmedArgs] : []),
      '</skill-loaded>',
    ];
    return { text: lines.join('\n'), name, path: posixPath(skillMd), source: 'project' };
  }

  /** Tolerant read for the engine pipeline: a malformed file contributes no servers. */
  private loadGlobalMcpConfig(): Record<string, StoredMcpServerConfig> {
    const mcpPath = join(this.homeDir, 'mcp.json');
    if (!existsSync(mcpPath)) return {};
    try {
      const raw = JSON.parse(readFileSync(mcpPath, 'utf8'));
      return (
        raw && typeof raw === 'object' && 'mcpServers' in raw ? raw.mcpServers : raw
      ) as Record<string, StoredMcpServerConfig>;
    } catch {
      return {};
    }
  }

  /**
   * Strict read for the CRUD paths: a malformed mcp.json must reject the
   * mutation instead of being silently overwritten (the file's bytes stay
   * untouched so the user can fix it).
   */
  private readGlobalMcpDocument(): { mcpServers: Record<string, StoredMcpServerConfig> } & Record<
    string,
    unknown
  > {
    const mcpPath = join(this.homeDir, 'mcp.json');
    if (!existsSync(mcpPath)) return { mcpServers: {} };
    let raw: unknown;
    try {
      raw = JSON.parse(readFileSync(mcpPath, 'utf8'));
    } catch (error) {
      throw new KimiError(
        ErrorCodes.CONFIG_INVALID,
        `Invalid mcp.json in ${this.homeDir}: ${error instanceof Error ? error.message : String(error)}`,
        { cause: error },
      );
    }
    if (!isPlainObject(raw)) {
      throw new KimiError(ErrorCodes.CONFIG_INVALID, `Invalid mcp.json in ${this.homeDir}`);
    }
    const servers = raw['mcpServers'];
    return {
      ...raw,
      mcpServers: isPlainObject(servers) ? (servers as Record<string, StoredMcpServerConfig>) : {},
    };
  }

  private saveGlobalMcpDocument(
    document: { mcpServers: Record<string, StoredMcpServerConfig> } & Record<string, unknown>,
  ): void {
    const mcpPath = join(this.homeDir, 'mcp.json');
    writeFileSync(mcpPath, JSON.stringify(document, null, 2), 'utf8');
  }

  /** Flatten one stored entry into the managed view (transport derived). */
  private toManagedServerInfo(name: string, config: StoredMcpServerConfig): McpManagedServerInfo {
    const transport = config.transport ?? (config.command !== undefined ? 'stdio' : 'http');
    return {
      ...config,
      transport,
      name,
      source: 'global' as const,
      origin: posixPath(join(this.homeDir, 'mcp.json')),
      mutable: true,
    } as McpManagedServerInfo;
  }

  override async listGlobalMcpServers(
    _options: { readonly cwd?: string } = {},
  ): Promise<readonly McpManagedServerInfo[]> {
    const servers = this.loadGlobalMcpConfig();
    return Object.entries(servers).map(([name, config]) => this.toManagedServerInfo(name, config));
  }

  override async getGlobalMcpServer(
    name: string,
    _options: { readonly cwd?: string } = {},
  ): Promise<McpManagedServerInfo> {
    const servers = this.loadGlobalMcpConfig();
    const config = servers[name];
    if (!config) {
      throw new KimiError(ErrorCodes.MCP_SERVER_NOT_FOUND, `MCP server "${name}" not found`);
    }
    return this.toManagedServerInfo(name, config);
  }

  override async addGlobalMcpServer(
    server: McpServerConfig,
    options: { readonly cwd?: string } = {},
  ): Promise<readonly McpManagedServerInfo[]> {
    const document = this.readGlobalMcpDocument();
    const name = server.name ?? `server_${randomUUID()}`;
    // The name is the entry key; the stored config never repeats it.
    const { name: _stripped, ...stored } = server as McpServerConfig & { name?: string };
    void _stripped;
    document.mcpServers[name] = stored;
    this.saveGlobalMcpDocument(document);
    return this.listGlobalMcpServers(options);
  }

  override async updateGlobalMcpServer(
    server: McpServerConfig,
    options: { readonly cwd?: string } = {},
  ): Promise<readonly McpManagedServerInfo[]> {
    return this.addGlobalMcpServer(server, options);
  }

  override async removeGlobalMcpServer(
    name: string,
    options: { readonly cwd?: string } = {},
  ): Promise<readonly McpManagedServerInfo[]> {
    const document = this.readGlobalMcpDocument();
    delete document.mcpServers[name];
    this.saveGlobalMcpDocument(document);
    return this.listGlobalMcpServers(options);
  }

  override async listGlobalMcpServerAuthStatuses(
    options: { readonly cwd?: string; readonly verify?: boolean } = {},
  ): Promise<readonly GlobalMcpServerAuthStatus[]> {
    const servers = this.loadGlobalMcpConfig();
    const statuses: GlobalMcpServerAuthStatus[] = [];
    for (const [name, config] of Object.entries(servers)) {
      statuses.push({
        name,
        authStatus: await this.resolveMcpServerAuthStatus(name, config, options.verify),
      });
    }
    return statuses;
  }

  /**
   * The engine's auth-posture classification for one user-global server:
   * stdio/bearer/header entries are statically classified, OAuth-candidate
   * entries consult the stored token state and (when verifying) probe the
   * endpoint with a real connection.
   */
  private async resolveMcpServerAuthStatus(
    name: string,
    server: StoredMcpServerConfig,
    verify: boolean | undefined,
  ): Promise<GlobalMcpServerAuthStatus['authStatus']> {
    if (server.enabled === false) return 'not-applicable';
    if (server.transport === 'stdio' || (server.transport === undefined && server.command)) {
      return 'not-applicable';
    }
    if (server.bearerTokenEnvVar !== undefined) return 'bearer-token';
    if (server.headers !== undefined && server.auth !== 'oauth') return 'not-applicable';
    if (server.transport !== 'http' && server.auth !== 'oauth') return 'not-applicable';
    const tokens = await this.readMcpOAuthTokens(name, server.url ?? '');
    const offline = (): GlobalMcpServerAuthStatus['authStatus'] => {
      if (tokens.hasTokens) {
        return !tokens.expired || tokens.hasRefreshToken ? 'oauth-authorized' : 'oauth-expired';
      }
      return server.auth === 'oauth' ? 'oauth-required' : 'not-applicable';
    };
    if (verify !== true) {
      if (verify === false || tokens.hasTokens || server.auth === 'oauth') return offline();
    }
    const probe = await this.probeRemoteMcpServer(server, tokens.accessToken ?? undefined);
    if (probe.status === 'connected') {
      return tokens.hasTokens ? 'oauth-authorized' : 'not-applicable';
    }
    if (probe.status === 'needs-auth') {
      return tokens.hasTokens ? 'oauth-expired' : 'oauth-required';
    }
    return offline();
  }

  /** Directory holding the engine's encrypted MCP OAuth records. */
  private mcpOAuthDir(): string {
    return join(this.homeDir, 'credentials', 'mcp');
  }

  /** The session's merged MCP config; project layers require workspace trust. */
  private async loadSessionMcpConfig(
    workDir: string,
  ): Promise<Record<string, StoredMcpServerConfig>> {
    const { trusted } = await this.getWorkspaceTrustInfo(workDir);
    return loadMcpServerLayers({
      homeDir: this.homeDir,
      workDir,
      includeProject: trusted,
    });
  }

  /** The engine stub: storage, decryption and refresh all live in Rust. */
  private async engineMcpOAuth(): Promise<{
    mcpOauthTokenState(name: string, url: string, dir: string): Promise<string>;
    mcpOauthStoreTokens(
      name: string,
      url: string,
      tokensJson: string,
      dir: string,
    ): string;
    mcpOauthRemove(name: string, url: string, dir: string): boolean;
    mcpOauthReconnect(name: string): Promise<number>;
  }> {
    return (await import('@moonshot-ai/kimi-agent/native')) as unknown as {
      mcpOauthTokenState(name: string, url: string, dir: string): Promise<string>;
      mcpOauthStoreTokens(
        name: string,
        url: string,
        tokensJson: string,
        dir: string,
      ): string;
      mcpOauthRemove(name: string, url: string, dir: string): boolean;
      mcpOauthReconnect(name: string): Promise<number>;
    };
  }

  /** The stored MCP OAuth token state for one server (engine-decrypted). */
  private async readMcpOAuthTokens(
    name: string,
    url: string,
  ): Promise<{
    hasTokens: boolean;
    expired: boolean;
    hasRefreshToken: boolean;
    accessToken: string | undefined;
  }> {
    try {
      const native = await this.engineMcpOAuth();
      const raw = await native.mcpOauthTokenState(name, url, this.mcpOAuthDir());
      const parsed = JSON.parse(raw) as {
        hasTokens?: boolean;
        expired?: boolean;
        hasRefreshToken?: boolean;
        accessToken?: string | null;
      };
      return {
        hasTokens: parsed.hasTokens === true,
        expired: parsed.expired === true,
        hasRefreshToken: parsed.hasRefreshToken === true,
        accessToken: parsed.accessToken ?? undefined,
      };
    } catch {
      return { hasTokens: false, expired: false, hasRefreshToken: false, accessToken: undefined };
    }
  }

  override async inspectAppMcpServers(
    _targets?: readonly McpServerLocator[],
    _options: { readonly cwd?: string } = {},
  ): Promise<readonly AppMcpServerInspection[]> {
    return [];
  }

  override async listMcpServers(input: SessionIdRpcInput): Promise<readonly McpServerInfo[]> {
    const meta = this.requireSession(input.sessionId);
    if (meta.handle === undefined) return [];
    // The engine connected these from `params.mcp_servers`; the host used to
    // answer `[]` unconditionally, so a configured server was invisible to
    // `/mcp` and to the VS Code MCP panel.
    const entries = await meta.handle.mcpServers();
    // The engine serialises its own `McpServerEntry` (`tool_count`, `tools` as
    // `{name, description}`); the public shape is camelCase, so passing the raw
    // entry through left `/mcp` reporting "0 tools" for every connected server.
    return entries.map((entry) => {
      const raw = entry as Record<string, unknown>;
      const tools = Array.isArray(raw['tools']) ? (raw['tools'] as readonly unknown[]) : undefined;
      return {
        ...raw,
        name: String(raw['name']),
        transport: String(raw['transport']),
        status: raw['status'] as McpServerInfo['status'],
        toolCount: typeof raw['tool_count'] === 'number' ? raw['tool_count'] : (tools?.length ?? 0),
        tools,
      } as McpServerInfo;
    });
  }

  override async suggestFiles(
    workDir: string,
    input: SuggestFilesInput,
  ): Promise<SuggestFilesResult | undefined> {
    // The engine's own search (`server::fs_routes::search_files`), reached
    // through the addon so the host and the HTTP server rank identically. The
    // wire spells the highlight frame `match_positions`; the public shape is
    // camelCase.
    const { fsSuggest } = await import('@moonshot-ai/kimi-agent/native');
    const raw = JSON.parse(fsSuggest(workDir, input.query, input.limit ?? 50)) as {
      items: readonly {
        path: string;
        name: string;
        kind: string;
        match_positions?: readonly number[];
      }[];
      truncated: boolean;
    };
    return {
      items: raw.items.map((item) => ({
        path: item.path,
        name: item.name,
        kind: item.kind as SuggestFilesItem['kind'],
        matchPositions: item.match_positions ?? [],
      })),
      truncated: raw.truncated,
    };
  }

  override async listWorkspaceMcpServers(_workDir: string): Promise<readonly McpServerInfo[]> {
    // No session exists yet, so nothing is connected and the engine holds no
    // registry to read. Report what a session would start, from the same
    // `mcp.json` the engine is handed at creation (`resolveMcpServersForEngine`).
    // Status is `pending`: the engine connects these when it builds a session
    // pipeline, so claiming `connected` here would be a lie.
    return resolveMcpServersForEngine(this.loadGlobalMcpConfig()).map((server) => ({
      name: server.name,
      transport: server.transport,
      status: 'pending' as const,
      toolCount: 0,
    }));
  }

  override async addSessionMcpServer(input: {
    readonly sessionId: string;
    readonly server: McpServerConfig;
    readonly persist?: boolean;
  }): Promise<McpServerInfo> {
    this.requireSession(input.sessionId);
    const name = input.server.name ?? `server_${randomUUID()}`;
    if (input.persist) {
      await this.addGlobalMcpServer(input.server);
    }
    // `pending`, not `connected`: this only records the configuration. The
    // engine connects servers when it builds a session pipeline from
    // `params.mcp_servers`, so nothing is connected yet — reporting
    // `connected` with `toolCount: 0` claimed a live server that did not
    // exist.
    return {
      name,
      transport: input.server.transport,
      status: 'pending',
      toolCount: 0,
    };
  }

  override async testGlobalMcpServer(
    name: string,
    _options: { readonly cwd?: string } = {},
  ): Promise<McpTestResult> {
    const servers = this.loadGlobalMcpConfig();
    const config = servers[name];
    if (!config) {
      throw new KimiError(ErrorCodes.MCP_SERVER_NOT_FOUND, `MCP server "${name}" not found`);
    }
    return this.testGlobalMcpServerConfig(config as McpServerConfig);
  }

  override async testGlobalMcpServerConfig(
    server: McpServerConfig,
    _options: { readonly cwd?: string } = {},
  ): Promise<McpTestResult> {
    const stored = server as StoredMcpServerConfig;
    if (stored.transport === 'http' || stored.transport === 'sse') {
      return this.probeRemoteMcpServer(stored);
    }
    return this.probeStdioMcpServer(stored);
  }

  /** Connect to a stdio MCP server and list its tools (the standalone check). */
  private async probeStdioMcpServer(server: StoredMcpServerConfig): Promise<McpTestResult> {
    const { Client } = await import('@modelcontextprotocol/sdk/client/index.js');
    const { StdioClientTransport } = await import('@modelcontextprotocol/sdk/client/stdio.js');
    const transport = new StdioClientTransport({
      command: server.command ?? '',
      ...(server.args ? { args: server.args } : {}),
      ...(server.env ? { env: server.env } : {}),
    });
    const client = new Client({ name: 'kimi-code-mcp-check', version: '0.0.0' });
    try {
      await client.connect(transport);
      const tools = await client.listTools();
      const lines = [
        'Connected to MCP server.',
        `Available tools: ${String(tools.tools.length)}`,
        ...tools.tools.map(
          (tool) => `- ${tool.name}${tool.description ? `: ${tool.description}` : ''}`,
        ),
      ];
      return { success: true, output: lines.join('\n') };
    } catch (error) {
      return {
        success: false,
        output: error instanceof Error ? error.message : String(error),
      };
    } finally {
      await client.close().catch(() => undefined);
    }
  }

  /**
   * Probe a remote MCP endpoint far enough to classify its auth posture: a
   * 200 initialize answer connects, a 401 challenge means needs-auth, and
   * anything else is an unreachable server.
   */
  private async probeRemoteMcpServer(
    server: StoredMcpServerConfig,
    bearerToken?: string,
  ): Promise<McpTestResult & { status: 'connected' | 'needs-auth' | 'unreachable' }> {
    const url = server.url ?? '';
    try {
      const response = await fetch(url, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          accept: 'application/json, text/event-stream',
          ...(bearerToken !== undefined ? { authorization: `Bearer ${bearerToken}` } : {}),
          ...server.headers,
        },
        body: JSON.stringify({
          jsonrpc: '2.0',
          id: 1,
          method: 'initialize',
          params: {
            protocolVersion: '2025-03-26',
            capabilities: {},
            clientInfo: { name: 'kimi-code-mcp-check', version: '0.0.0' },
          },
        }),
        signal: AbortSignal.timeout(5_000),
      });
      if (response.status === 401 || response.status === 403) {
        return { success: false, output: 'authorization required', status: 'needs-auth' };
      }
      if (!response.ok) {
        return {
          success: false,
          output: `MCP server responded with HTTP ${String(response.status)}`,
          status: 'unreachable',
        };
      }
      return { success: true, output: 'Connected to MCP server.', status: 'connected' };
    } catch (error) {
      return {
        success: false,
        output: error instanceof Error ? error.message : String(error),
        status: 'unreachable',
      };
    }
  }

  override async beginGlobalMcpServerAuth(
    name: string,
    _options: { readonly cwd?: string } = {},
  ): Promise<BeginGlobalMcpServerAuthResult> {
    const servers = this.loadGlobalMcpConfig();
    const config = servers[name];
    if (
      config &&
      (config.transport === 'stdio' || (config.transport === undefined && config.command))
    ) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `MCP server "${name}" uses stdio transport and does not support OAuth authorization`,
      );
    }
    const url = config?.url;
    if (config === undefined || url === undefined) {
      throw new KimiError(
        ErrorCodes.MCP_SERVER_NOT_FOUND,
        `MCP server "${name}" is not a configured remote server`,
      );
    }
    // A stored grant that is still usable (or refreshable) needs no browser.
    const state = await this.readMcpOAuthTokens(name, url);
    if (state.hasTokens && (!state.expired || state.hasRefreshToken)) {
      return { status: 'already-authorized' };
    }
    const started = await beginMcpOAuthFlow({
      name,
      serverUrl: url,
      clientName: 'Kimi Code',
      persist: async (tokens) => {
        const native = await this.engineMcpOAuth();
        native.mcpOauthStoreTokens(name, url, JSON.stringify(tokens), this.mcpOAuthDir());
      },
    });
    return {
      status: 'authorization-required',
      flowId: started.flowId,
      authorizationUrl: started.authorizationUrl,
    };
  }

  override async completeGlobalMcpServerAuth(
    input: { readonly flowId: string; readonly timeoutMs?: number },
    signal?: AbortSignal,
  ): Promise<void> {
    const { name } = await completeMcpOAuthFlow(input.flowId, {
      timeoutMs: input.timeoutMs,
      signal,
    });
    // The server was marked needs-auth by the failed connection; retry it in
    // every live manager so the fresh grant takes effect without a new session.
    try {
      const native = await this.engineMcpOAuth();
      await native.mcpOauthReconnect(name);
    } catch {
      // A host without live managers still completed the login.
    }
  }

  override async cancelGlobalMcpServerAuth(flowId: string): Promise<void> {
    await cancelMcpOAuthFlow(flowId);
  }

  override async resetGlobalMcpServerAuth(
    name: string,
    _options: { readonly cwd?: string } = {},
  ): Promise<void> {
    const config = this.loadGlobalMcpConfig()[name];
    if (config?.url === undefined) return;
    const native = await this.engineMcpOAuth();
    native.mcpOauthRemove(name, config.url, this.mcpOAuthDir());
  }

  /**
   * Authorize a *workspace-scoped* MCP server (a project `.mcp.json` entry
   * resolved through {@link loadSessionMcpConfig}). The browser flow is the
   * global one; only the config source differs — the locator-addressed variant
   * exists so a session/plugin panel can authenticate a server that is not in
   * the user-global config (v2 `beginMcpServerAuth({ locator })`).
   *
   * A locator with a `pluginId` addresses a plugin-contributed server
   * instead; its manifest is engine-owned, so that branch reads it through
   * `getPluginInfo` (see {@link beginPluginMcpServerAuth}).
   */
  override async beginMcpServerAuth(
    locator: McpServerLocator,
    options: { readonly cwd?: string } = {},
  ): Promise<BeginGlobalMcpServerAuthResult> {
    if (locator.pluginId !== undefined && locator.pluginId.length > 0) {
      return this.beginPluginMcpServerAuth(locator.pluginId, locator.serverName);
    }
    const workDir = options.cwd ?? process.cwd();
    const servers = await this.loadSessionMcpConfig(workDir);
    const config = servers[locator.serverName];
    if (
      config !== undefined &&
      (config.transport === 'stdio' || (config.transport === undefined && config.command))
    ) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `MCP server "${locator.serverName}" uses stdio transport and does not support OAuth authorization`,
      );
    }
    const url = config?.url;
    if (config === undefined || url === undefined) {
      throw new KimiError(
        ErrorCodes.MCP_SERVER_NOT_FOUND,
        `MCP server "${locator.serverName}" is not a configured remote server in ${workDir}`,
      );
    }
    // A stored grant that is still usable (or refreshable) needs no browser.
    const state = await this.readMcpOAuthTokens(locator.serverName, url);
    if (state.hasTokens && (!state.expired || state.hasRefreshToken)) {
      return { status: 'already-authorized' };
    }
    const started = await beginMcpOAuthFlow({
      name: locator.serverName,
      serverUrl: url,
      clientName: 'Kimi Code',
      persist: async (tokens) => {
        const native = await this.engineMcpOAuth();
        native.mcpOauthStoreTokens(
          locator.serverName,
          url,
          JSON.stringify(tokens),
          this.mcpOAuthDir(),
        );
      },
    });
    return {
      status: 'authorization-required',
      flowId: started.flowId,
      authorizationUrl: started.authorizationUrl,
    };
  }

  /**
   * Authorize a plugin-contributed remote server (v2's plugin locator). The
   * manifest is engine-owned, so it is read through `getPluginInfo`; the grant
   * is stored under the engine's runtime name (`<pluginId>__<name>`) because
   * that is the name the session's MCP manager registers the server under.
   */
  private async beginPluginMcpServerAuth(
    pluginId: string,
    serverName: string,
  ): Promise<BeginGlobalMcpServerAuthResult> {
    const info = await this.getPluginInfo(pluginId);
    const server = info.mcpServers?.find((entry) => entry.name === serverName);
    if (server === undefined) {
      throw new KimiError(
        ErrorCodes.MCP_SERVER_NOT_FOUND,
        `Plugin "${pluginId}" does not declare an MCP server named "${serverName}"`,
      );
    }
    if (server.transport === 'stdio' || server.command !== undefined) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `MCP server "${serverName}" uses stdio transport and does not support OAuth authorization`,
      );
    }
    const url = server.url;
    if (url === undefined) {
      throw new KimiError(
        ErrorCodes.MCP_SERVER_NOT_FOUND,
        `MCP server "${serverName}" is not a configured remote server`,
      );
    }
    const runtimeName = server.runtimeName ?? `${pluginId}__${serverName}`;
    const state = await this.readMcpOAuthTokens(runtimeName, url);
    if (state.hasTokens && (!state.expired || state.hasRefreshToken)) {
      return { status: 'already-authorized' };
    }
    const started = await beginMcpOAuthFlow({
      name: runtimeName,
      serverUrl: url,
      clientName: 'Kimi Code',
      persist: async (tokens) => {
        const native = await this.engineMcpOAuth();
        native.mcpOauthStoreTokens(runtimeName, url, JSON.stringify(tokens), this.mcpOAuthDir());
      },
    });
    return {
      status: 'authorization-required',
      flowId: started.flowId,
      authorizationUrl: started.authorizationUrl,
    };
  }

  override async completeMcpServerAuth(
    input: { readonly flowId: string; readonly timeoutMs?: number },
    signal?: AbortSignal,
  ): Promise<void> {
    const { name } = await completeMcpOAuthFlow(input.flowId, {
      timeoutMs: input.timeoutMs,
      signal,
    });
    try {
      const native = await this.engineMcpOAuth();
      await native.mcpOauthReconnect(name);
    } catch {
      // A host without live managers still completed the login.
    }
  }

  override async cancelMcpServerAuth(flowId: string): Promise<void> {
    await cancelMcpOAuthFlow(flowId);
  }

  /** Drop the workspace-scoped grant for a locator-addressed server. */
  override async resetMcpServerAuth(
    locator: McpServerLocator,
    options: { readonly cwd?: string } = {},
  ): Promise<void> {
    if (locator.pluginId !== undefined && locator.pluginId.length > 0) {
      const info = await this.getPluginInfo(locator.pluginId);
      const server = info.mcpServers?.find((entry) => entry.name === locator.serverName);
      const url = server?.url;
      if (url === undefined) return;
      const runtimeName = server?.runtimeName ?? `${locator.pluginId}__${locator.serverName}`;
      const native = await this.engineMcpOAuth();
      native.mcpOauthRemove(runtimeName, url, this.mcpOAuthDir());
      return;
    }
    const workDir = options.cwd ?? process.cwd();
    const config = (await this.loadSessionMcpConfig(workDir))[locator.serverName];
    if (config?.url === undefined) return;
    const native = await this.engineMcpOAuth();
    native.mcpOauthRemove(locator.serverName, config.url, this.mcpOAuthDir());
  }

  /**
   * The engine pipeline's task runner (Rust `TaskRunner`, attached to the
   * process-wide subagent manager at build time) is the backing store:
   * entries are the engine's background subagent runs (`kind: 'agent'`,
   * v2 task-domain wire: taskId / description / status / startedAt /
   * endedAt / stopReason).
   */
  override async listBackgroundTasks(
    input: SessionIdRpcInput & { activeOnly?: boolean; limit?: number },
  ): Promise<readonly BackgroundTaskInfo[]> {
    this.requireSession(input.sessionId);
    const { backgroundTaskList } = await import('@moonshot-ai/kimi-agent/native');
    let raw: unknown;
    try {
      raw = JSON.parse(backgroundTaskList());
    } catch (error) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `malformed background task list from the engine: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
    if (!Array.isArray(raw)) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        'malformed background task list from the engine',
      );
    }
    // The engine owns the entry shape; validate just enough that a bad
    // payload fails here with a named error instead of poisoning consumers
    // downstream (status stays a cast — the v2 domain owns its vocabulary).
    let tasks: BackgroundTaskInfo[] = (raw as readonly EngineTaskWireEntry[]).map((entry) => {
      if (typeof entry !== 'object' || entry === null) {
        throw new KimiError(
          ErrorCodes.REQUEST_INVALID,
          'malformed background task entry from the engine',
        );
      }
      return {
        kind: 'agent',
        taskId: String(entry.taskId ?? ''),
        description: String(entry.description ?? ''),
        status: entry.status as BackgroundTaskInfo['status'],
        startedAt: Number(entry.startedAt ?? 0),
        endedAt: entry.endedAt ?? null,
        ...(entry.stopReason !== undefined ? { stopReason: entry.stopReason } : {}),
      };
    });
    if (input.activeOnly) {
      tasks = tasks.filter((task) => task.status === 'running');
    }
    if (input.limit !== undefined && input.limit > 0 && tasks.length > input.limit) {
      tasks = tasks.slice(tasks.length - input.limit);
    }
    return tasks;
  }

  /**
   * Switch the active web search engine at runtime (manual hot-switch).
   * `engine` is `"bing"` or `"ddg"`. Returns the active engine name.
   */
  override async setSearchEngine(engine: string): Promise<string> {
    const { setSearchEngine } = await import('@moonshot-ai/kimi-agent/native');
    return setSearchEngine(engine);
  }

  override async getBackgroundTaskOutput(
    input: SessionIdRpcInput & { taskId: string; tail?: number },
  ): Promise<string> {
    this.requireSession(input.sessionId);
    const { backgroundTaskOutput } = await import('@moonshot-ai/kimi-agent/native');
    const output = backgroundTaskOutput(input.taskId) ?? '';
    // `tail` caps to trailing characters (the `getBackgroundTaskOutput`
    // contract); the engine returns the full snapshot.
    if (input.tail === undefined || input.tail < 0) return output;
    return input.tail === 0 ? '' : output.slice(-input.tail);
  }

  override async stopBackgroundTask(
    input: SessionIdRpcInput & { taskId: string; reason?: string },
  ): Promise<void> {
    this.requireSession(input.sessionId);
    const { backgroundTaskStop } = await import('@moonshot-ai/kimi-agent/native');
    await backgroundTaskStop(input.taskId, input.reason);
  }

  override async detachBackgroundTask(
    input: SessionIdRpcInput & { taskId: string },
  ): Promise<BackgroundTaskInfo | undefined> {
    // The engine task runner is process-global: its tasks already outlive the
    // session handle, so there is no separate detach transition to record.
    // The session check keeps the guard consistent with list/get/stop.
    this.requireSession(input.sessionId);
    return undefined;
  }

  /**
   * Nothing to wait for: the engine drains the session's background tasks
   * inside the turn, while the turn slot is still held, and only then resolves
   * the `prompt()` receipt. By the time a host could call this, the drain has
   * already happened — the engine's own comment says holding the receipt is
   * what keeps the host free of a settle loop.
   */
  override async waitForBackgroundTasksOnPrint(_input: SessionIdRpcInput): Promise<void> {}

  /**
   * `'finish'` is the answer, not a placeholder. The engine runs the whole
   * print-background lifecycle inside the turn — drain, then the goal / cron /
   * task follow-up turns for `steer` mode — so a completed main turn means the
   * run is done and the host has nothing to keep alive.
   */
  override async handlePrintMainTurnCompleted(
    _input: SessionIdRpcInput,
  ): Promise<'finish' | 'continue'> {
    return 'finish';
  }

  override async listCommands(_input: SessionIdRpcInput): Promise<readonly AgentCommandInfo[]> {
    return [];
  }

  override async runCommand(_input: unknown): Promise<void> {
    throw new KimiError(
      ErrorCodes.NOT_IMPLEMENTED,
      'This SDK client does not support contributed commands.',
    );
  }

  /**
   * Open the engine's plugin registry against the SQLite store the app-scope
   * engine uses (`<engineDataDir>/sessions.db`), so the CLI and the hosted
   * server read one install state. Idempotent; every plugin method below calls
   * it first.
   *
   * Before the store opens, the built-in `normify` plugin is materialized from
   * the embedded copy and seeded into the registry when absent, so the CLI
   * ships with it enabled by default (zero install). A user's disable is
   * preserved (the seeder never re-enables) and an explicit removal is
   * remembered via the opt-out marker.
   */
  private async ensurePluginStore(): Promise<void> {
    if (this.pluginStoreReady) return;
    const { initPluginStore } = await import('@moonshot-ai/kimi-agent/native');
    const seededRoot = materializeBuiltinPlugins(this.engineDataDir);
    initPluginStore(
      this.engineDataDir,
      resolvePluginMarketplaceDir(),
      // Only the packaged single-file binary can re-execute itself as
      // `__plugin_run_node`; source (bun) and node-dist runs keep spawning the
      // `node` that a JS plugin server declares.
      pluginNodeRunnerPath(),
    );
    if (seededRoot !== undefined) await this.seedBuiltinNormify(seededRoot);
    this.pluginStoreReady = true;
  }

  /**
   * Install the built-in normify plugin from its materialized local copy when
   * the registry has no record of it. The engine defaults a fresh install to
   * enabled; an existing record (enabled or user-disabled) is left exactly as
   * it is, so the seeder never overrides a `/plugins` toggle. Failures are
   * swallowed: a broken seed must not take down the plugin API.
   */
  private async seedBuiltinNormify(seededRoot: string): Promise<void> {
    try {
      const { pluginInfo, pluginInstall } = await import('@moonshot-ai/kimi-agent/native');
      const existing = pluginInfo(BUILTIN_NORMIFY_PLUGIN_ID);
      if (existing !== null && existing !== undefined) return;
      pluginInstall(seededRoot);
    } catch (error) {
      console.warn(
        `builtin plugin seed failed: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }

  override async listPlugins(): Promise<readonly PluginSummary[]> {
    await this.ensurePluginStore();
    const { pluginList } = await import('@moonshot-ai/kimi-agent/native');
    return JSON.parse(pluginList()) as readonly PluginSummary[];
  }

  override async installPlugin(source: string): Promise<PluginSummary> {
    await this.ensurePluginStore();
    const { pluginInstall } = await import('@moonshot-ai/kimi-agent/native');
    // The catalog keys installs by id, but a caller may hand over the catalog
    // `source` (the TUI resolves a path or URL before calling); the engine
    // matches either.
    const raw = pluginInstall(source.trim());
    if (raw === null || raw === undefined) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `Unknown plugin "${source}" — it is not in the marketplace catalog.`,
      );
    }
    return JSON.parse(raw) as PluginSummary;
  }

  override async getPluginInfo(id: string): Promise<PluginInfo> {
    await this.ensurePluginStore();
    const { pluginInfo } = await import('@moonshot-ai/kimi-agent/native');
    const raw = pluginInfo(id);
    if (raw === null || raw === undefined) {
      throw new KimiError(ErrorCodes.REQUEST_INVALID, `Plugin "${id}" is not installed.`);
    }
    return JSON.parse(raw) as PluginInfo;
  }

  override async setPluginEnabled(id: string, enabled: boolean): Promise<void> {
    await this.ensurePluginStore();
    const { pluginSetEnabled } = await import('@moonshot-ai/kimi-agent/native');
    if (!pluginSetEnabled(id, enabled)) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `Plugin "${id}" is neither installed nor in the marketplace catalog.`,
      );
    }
    // Re-enabling the built-in normify plugin clears the removal opt-out: the
    // toggle is the newer user decision and it wins over the marker.
    if (id === BUILTIN_NORMIFY_PLUGIN_ID && enabled) {
      setBuiltinPluginOptOut(this.engineDataDir, false);
    }
  }

  override async setPluginMcpServerEnabled(
    id: string,
    server: string,
    enabled: boolean,
  ): Promise<void> {
    await this.ensurePluginStore();
    const { pluginSetMcpServerEnabled } = await import('@moonshot-ai/kimi-agent/native');
    if (!pluginSetMcpServerEnabled(id, server, enabled)) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `Plugin "${id}" does not declare an MCP server named "${server}".`,
      );
    }
  }

  override async removePlugin(id: string): Promise<void> {
    await this.ensurePluginStore();
    const { pluginRemove } = await import('@moonshot-ai/kimi-agent/native');
    if (!pluginRemove(id)) {
      throw new KimiError(ErrorCodes.REQUEST_INVALID, `Plugin "${id}" is not installed.`);
    }
    // Removing the built-in normify plugin is an explicit user decision: record
    // it so the seeder does not reinstall it on the next launch.
    if (id === BUILTIN_NORMIFY_PLUGIN_ID) setBuiltinPluginOptOut(this.engineDataDir, true);
  }

  override async reloadPlugins(): Promise<ReloadSummary> {
    await this.ensurePluginStore();
    const { pluginReload } = await import('@moonshot-ai/kimi-agent/native');
    return JSON.parse(pluginReload()) as ReloadSummary;
  }

  override async listPluginCommands(): Promise<readonly PluginCommandDef[]> {
    // Plugin commands are app-global: the engine reads them from the installed
    // plugins' manifests, not from a session.
    return this.listPluginCommandsGlobal();
  }

  override async listPluginCommandsGlobal(): Promise<readonly PluginCommandDef[]> {
    await this.ensurePluginStore();
    const { pluginCommands } = await import('@moonshot-ai/kimi-agent/native');
    return JSON.parse(pluginCommands()) as readonly PluginCommandDef[];
  }

  /**
   * Expand a plugin command's body into the prompt it submits. Mirrors the v2
   * `expandCommandArguments` contract: `$ARGUMENTS` is substituted in place,
   * and a body without the placeholder gets the args appended as an
   * `ARGUMENTS:` trailer rather than losing them.
   */
  override async activatePluginCommand(input: ActivatePluginCommandRpcInput): Promise<void> {
    const meta = this.requireSession(input.sessionId);
    const defs = await this.listPluginCommandsGlobal();
    const def = defs.find(
      (command) => command.pluginId === input.pluginId && command.name === input.commandName,
    );
    if (def === undefined) {
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `Plugin command "${input.pluginId}:${input.commandName}" was not found`,
      );
    }
    const body = def.body ?? def.prompt ?? '';
    const commandArgs = input.args ?? '';
    const expanded = expandCommandArguments(body, commandArgs);
    const activationId = `plugin_${randomUUID()}`;
    this.receiveEvent({
      sessionId: meta.id,
      agentId: 'main',
      type: 'plugin_command.activated',
      activationId,
      pluginId: input.pluginId,
      commandName: input.commandName,
      commandArgs,
      trigger: 'user-slash',
    });
    this.applyPromptMetadata(
      meta,
      promptMetadataTextFromText(
        commandArgs.length === 0
          ? `/${input.pluginId}:${input.commandName}`
          : `/${input.pluginId}:${input.commandName} ${commandArgs}`,
      ),
    );
    return this.prompt({
      sessionId: meta.id,
      input: [{ type: 'text', text: expanded }],
      skipPromptMetadata: true,
    });
  }

  override async getExperimentalFeatures(): Promise<readonly ExperimentalFeatureState[]> {
    return resolveExperimentalFeatures(loadRuntimeConfigLenient(this.configPath));
  }

  /**
   * The project-level MCP servers that trusting this workspace would enable,
   * in the shape the trust prompt renders.
   *
   * Read straight from the two project candidates rather than through
   * `listWorkspaceMcpServers`: that method ignores its `workDir` and answers
   * the *user-global* `mcp.json` (its own comment says so), so it would
   * disclose servers the user already chose instead of the ones this project
   * gates. Showing the wrong set here is worse than showing none: the prompt
   * asks the user to judge a directory on the strength of that list.
   *
   * Deliberately the safe subset. `env`, headers and anything else the config
   * carries are what a planted `.mcp.json` would exfiltrate, and this prompt
   * runs *before* the workspace is trusted, so they are not carried into it.
   * The renderer sanitizes control characters on top of that.
   */
  private readProjectMcpDisclosure(workDir: string): WorkspaceTrustMcpServerInfo[] {
    const candidates = [join(workDir, '.mcp.json'), join(workDir, '.kimi-code', 'mcp.json')];
    const disclosure: WorkspaceTrustMcpServerInfo[] = [];
    for (const file of candidates) {
      let parsed: { mcpServers?: Record<string, unknown> };
      try {
        parsed = JSON.parse(readFileSync(file, 'utf-8')) as {
          mcpServers?: Record<string, unknown>;
        };
      } catch {
        // Missing or unparseable: the same silent skip the vscode handler makes.
        continue;
      }
      const servers = parsed?.mcpServers;
      if (servers === null || typeof servers !== 'object') continue;
      for (const [name, raw] of Object.entries(servers)) {
        if (raw === null || typeof raw !== 'object') continue;
        const cfg = raw as Record<string, unknown>;
        const command = typeof cfg['command'] === 'string' ? cfg['command'] : undefined;
        const url = typeof cfg['url'] === 'string' ? cfg['url'] : undefined;
        const declared = cfg['transport'];
        const transport =
          declared === 'stdio' || declared === 'http' || declared === 'sse'
            ? declared
            : command !== undefined
              ? ('stdio' as const)
              : url !== undefined
                ? ('http' as const)
                : undefined;
        // Neither a command nor a url: there is nothing to describe, and
        // guessing a transport would put a claim in front of the user that the
        // config never made.
        if (transport === undefined) continue;
        const args = cfg['args'];
        disclosure.push({
          name,
          transport,
          command,
          args: Array.isArray(args) ? args.map(String) : undefined,
          cwd: typeof cfg['cwd'] === 'string' ? cfg['cwd'] : undefined,
          url,
        });
      }
    }
    return disclosure;
  }

  override async getWorkspaceTrustInfo(workDir: string): Promise<WorkspaceTrustInfo> {
    // Fail closed: a workspace is untrusted until the user explicitly trusts it.
    // The previous `trusted: true` stub silently disabled the TUI's "do you trust
    // this folder?" gate, so an unfamiliar checkout's workspace-level MCP servers
    // and skills were loaded and run without any prompt. gatedMcpServers stays
    // empty until the MCP catalog is wired; the trust decision itself is persisted
    // so the prompt is shown once, not on every launch.
    //
    // `KIMI_CODE_TRUST_WORKSPACE` (upstream #4059) ORs over the persisted
    // decision, so a non-interactive host can opt every workspace in without a
    // persisted record. It wins over the record, which means untrusting has no
    // observable effect while it is set — the same trade upstream made. Only the
    // truthy spellings count; `parseBooleanEnv` returns undefined for anything it
    // does not recognise, so a typo leaves the normal prompt in place instead of
    // silently trusting everything.
    const envTrusted = parseBooleanEnv(process.env['KIMI_CODE_TRUST_WORKSPACE']) === true;
    const trusted =
      envTrusted ||
      this.readTrustedWorkspaces().some(
        (entry) => workspaceTrustKey(entry) === workspaceTrustKey(workDir),
      );
    return { trusted, gatedMcpServers: this.readProjectMcpDisclosure(workDir) };
  }

  override async trustWorkspace(workDir: string): Promise<void> {
    const trusted = this.readTrustedWorkspaces();
    if (!trusted.some((entry) => workspaceTrustKey(entry) === workspaceTrustKey(workDir))) {
      trusted.push(workDir);
      this.writeTrustedWorkspaces(trusted);
    }
  }

  private readTrustedWorkspaces(): string[] {
    try {
      const raw = readFileSync(join(this.homeDir, 'trusted-workspaces.json'), 'utf8');
      const parsed = JSON.parse(raw) as unknown;
      return Array.isArray(parsed)
        ? parsed.filter((entry): entry is string => typeof entry === 'string')
        : [];
    } catch {
      return [];
    }
  }

  private writeTrustedWorkspaces(list: readonly string[]): void {
    writeFileSync(
      join(this.homeDir, 'trusted-workspaces.json'),
      JSON.stringify(list, null, 2),
      'utf8',
    );
  }

  private requireSession(sessionId: string): NativeSessionMeta {
    const meta = this.liveSessions.get(sessionId);
    if (meta === undefined) {
      throw new KimiError(ErrorCodes.SESSION_NOT_FOUND, `unknown session "${sessionId}"`, {
        details: { sessionId },
      });
    }
    return meta;
  }

  private recordTurnOutcome(meta: NativeSessionMeta, outcome: SessionTurnOutcome): void {
    const result = outcome.result;
    if (!result) return;
    // The engine owns the usage fold; refresh the cached snapshot it exposes
    // instead of re-summing the outcome here.
    void this.refreshUsage(meta);
    // The last turn's full prompt size is the best proxy the native handle
    // offers for live context occupancy (it exposes no direct token count).
    meta.contextTokens = result.inputTokens + result.inputCacheRead + result.inputCacheCreation;
    meta.messageCount += 1;
  }

  /**
   * Tear down and rebuild the engine handle after a mid-session setting change
   * (model / thinking / permission / swarm / tower), carrying the conversation
   * over via getHistory / setHistory so context survives. Disposing the old
   * handle cancels any in-flight turn — setters are a user-initiated
   * reconfiguration and the TUI blocks them while a turn is running.
   *
   * The new handle is built *before* the old one is disposed. The engine keys
   * its registry by a process-local id (`session-<n>`), so the two never
   * collide — and a build that throws leaves the session on its previous
   * handle. Disposing first left `meta.handle` undefined, and every later
   * prompt then reported `session.not_found` ("cannot prompt unknown or closed
   * session") for a session that was still live and still listed.
   */
  private async rebuildHandle(meta: NativeSessionMeta): Promise<void> {
    const previous = meta.handle;
    // A failed read is NOT an empty history: swallowing it started the
    // replacement handle with no context while the transcript still showed the
    // whole conversation. The throw lands before buildHandle, so the session
    // stays on its previous handle — the contract this method documents.
    const history = previous ? await previous.getHistory() : [];
    const handle = await this.buildHandle(meta);
    try {
      if (history.length > 0) {
        await handle.setHistory(history);
      }
    } catch (error) {
      // The replacement never took over; drop it rather than leaking a live
      // engine session the host can no longer address.
      await handle.dispose().catch(() => {});
      throw error;
    }
    meta.handle = handle;
    meta.updatedAt = Date.now();
    await this.persistHistory(meta);
    await previous?.dispose().catch(() => {});
  }

  /**
   * Fail session creation when `--agent` names a profile that resolves
   * nowhere. The name may be a built-in profile or one discovered from an
   * agent file; anything else is a typo, and the SDK must not fall back to the
   * default agent silently (`docs/en/customization/agents.md:140`).
   */
  private async assertAgentProfileResolves(meta: NativeSessionMeta): Promise<void> {
    const requested = meta.agentProfile;
    if (requested === undefined) return;
    const key = requested.trim().toLowerCase();
    if (BUILTIN_AGENT_PROFILE_NAMES.includes(key)) return;
    const discovered = await this.resolveSessionAgentProfiles(meta);
    if (discovered.some((profile) => profile.name.toLowerCase() === key)) return;
    const available = [
      ...BUILTIN_AGENT_PROFILE_NAMES,
      ...discovered.map((profile) => profile.name),
    ];
    throw new KimiError(
      ErrorCodes.AGENT_NOT_FOUND,
      `Unknown agent profile "${requested}". Available agents: ${available.join(', ')}`,
      { details: { agentProfile: requested, available } },
    );
  }

  /**
   * Discover the agent files visible to a session and project them onto the
   * engine's `subagentProfiles` wire (P46). The native `Agent` tool reads the
   * registered snapshot to spawn subagents; without this the host's profiles
   * never reach it.
   *
   * A malformed discovered file is skipped with a warning; an explicit
   * `--agent-file` is fatal, because the user named it on the command line.
   */
  private async resolveSessionAgentProfiles(
    meta: NativeSessionMeta,
  ): Promise<readonly AgentFileDefinition[]> {
    const config = loadRuntimeConfigLenient(this.configPath);
    let discovered: DiscoveredAgentFiles;
    try {
      discovered = discoverAgentFiles({
        workDir: meta.workDir,
        kimiHome: this.homeDir,
        osHomeDir: homedir(),
        extraDirs: config.extraAgentDirs,
        explicitFiles: meta.agentFiles,
        pluginAgentDirs: await this.resolvePluginAgentDirs(),
      });
    } catch (error) {
      // Only an explicit `--agent-file` read/parse failure throws here; every
      // other scope degrades to a warning. Fail creation naming the file.
      throw new KimiError(
        ErrorCodes.REQUEST_INVALID,
        `Cannot load --agent-file for session "${meta.id}": ${error instanceof Error ? error.message : String(error)}`,
        { cause: error },
      );
    }
    for (const warning of discovered.warnings) {
      log.warn(`agent file discovery: ${warning}`);
    }
    return discovered.profiles;
  }

  /**
   * The `agents` directories of enabled plugins: each manifest-declared `./`
   * path, or the plugin root's `agents/` when the manifest omits the field
   * (`docs/en/customization/plugins.md:409`). Plugin roots come from the
   * engine's plugin registry — the only side that installs and knows them.
   * Any failure degrades to "no plugin agents": a plugin problem must not
   * block session creation.
   *
   * The registry is only consulted when the host has already initialized it.
   * Opening it here would be a side effect of creating a session — it pins the
   * plugin SQLite file for the process lifetime, and on Windows that alone is
   * enough to make a caller's cleanup fail with EBUSY. Session creation must
   * not take a lock the caller never asked for, so an uninitialized store
   * simply contributes no plugin agents (the plugin panel initializes it when
   * the user opens it, and the next handle build picks the agents up).
   */
  private async resolvePluginAgentDirs(): Promise<readonly string[]> {
    if (!this.pluginStoreReady) return [];
    try {
      const native = await import('@moonshot-ai/kimi-agent/native');
      const summaries = JSON.parse(native.pluginList()) as readonly PluginSummary[];
      const dirs: string[] = [];
      for (const summary of summaries) {
        if (!summary.enabled) continue;
        const raw = native.pluginInfo(summary.id);
        if (raw === null || raw === undefined) continue;
        const info = JSON.parse(raw) as PluginInfo;
        if (typeof info.root !== 'string' || info.root.length === 0) continue;
        for (const entry of manifestAgentPaths(info)) {
          dirs.push(resolve(info.root, entry));
        }
      }
      return dirs;
    } catch (error) {
      log.warn('plugin agent discovery failed; continuing without plugin agents', {
        error: error instanceof Error ? error.message : String(error),
      });
      return [];
    }
  }

  /**
   * Apply a setting that is baked into the engine handle at build time, and
   * roll the field back when the rebuild fails. Without the rollback a failed
   * `setModel` left `meta.model` naming a model the engine never switched to,
   * so the TUI reported a change that had not happened.
   *
   * The new value must also be persisted: the handle is rebuilt from `meta`,
   * so a resume that reads a stale `session-meta.json` builds the engine with
   * the old value while the replay (which reads the engine's own recorded
   * state) shows the new one — a permission mode switched mid-conversation
   * came back as the previous mode and prompted for tools the user had already
   * allowed.
   */ private async applyRebuiltSetting<K extends keyof NativeSessionMeta>(
    meta: NativeSessionMeta,
    key: K,
    value: NativeSessionMeta[K],
  ): Promise<void> {
    const previous = meta[key];
    meta[key] = value;
    try {
      await this.rebuildHandle(meta);
    } catch (error) {
      meta[key] = previous;
      throw error;
    }
    this.persistMeta(meta);
  }

  private persistMeta(meta: NativeSessionMeta): void {
    const persisted: PersistedSessionMeta = {
      id: meta.id,
      workDir: meta.workDir,
      createdAt: meta.createdAt,
      updatedAt: meta.updatedAt,
      title: meta.title,
      isCustomTitle: meta.isCustomTitle,
      lastPrompt: meta.lastPrompt,
      custom: meta.custom,
      additionalDirs: meta.additionalDirs,
      model: meta.model,
      thinkingEffort: meta.thinkingEffort,
      permissionMode: meta.permissionMode,
      planMode: meta.planMode,
      plan: meta.plan,
      goal: meta.goal,
      forkedFrom: meta.forkedFrom,
      contextTokens: meta.contextTokens,
      usageJson: meta.usageJson,
      agents: meta.agents,
      agentProfile: meta.agentProfile,
      agentFiles: meta.agentFiles,
    };
    this.writePersistedMeta(meta.sessionDir, persisted);
  }

  private writePersistedMeta(sessionDir: string, persisted: PersistedSessionMeta): void {
    try {
      mkdirSync(sessionDir, { recursive: true });
      writeFileSync(
        join(sessionDir, 'session-meta.json'),
        JSON.stringify(persisted, null, 2),
        'utf8',
      );
    } catch {
      // Best-effort durability: a metadata write failure must not break the
      // in-memory mutation the caller already relies on.
    }
  }

  private loadMeta(sessionDir: string): PersistedSessionMeta | undefined {
    try {
      const raw = readFileSync(join(sessionDir, 'session-meta.json'), 'utf8');
      const parsed = JSON.parse(raw) as PersistedSessionMeta;
      return parsed && typeof parsed.id === 'string' ? parsed : undefined;
    } catch {
      return undefined;
    }
  }

  /** Map a prompt submission onto the engine's serialized LLM message. */
  private toSessionPrompt(input: SessionPromptRpcInput['input'], origin?: unknown): SessionPrompt {
    if (typeof input === 'string') {
      return { role: 'user', content: input, ...(origin === undefined ? {} : { origin }) };
    }
    return this.toSessionPromptFromParts(input, origin);
  }

  private toSessionPromptFromParts(parts: readonly PromptPart[], origin?: unknown): SessionPrompt {
    const text = parts
      .filter((part): part is Extract<PromptPart, { type: 'text' }> => part.type === 'text')
      .map((part) => part.text)
      .join('\n');
    const blocks = parts.map((part) => {
      if (part.type === 'text') {
        return { type: 'text', text: part.text };
      }
      if (part.type === 'image_url') {
        return { type: 'image_url', url: part.imageUrl.url };
      }
      if (part.type === 'video_url') {
        return { type: 'video_url', url: part.videoUrl.url };
      }
      return part;
    });
    return {
      role: 'user',
      content: text,
      blocks,
      // Multi-part messages carry their parts structurally so the context
      // projection (and the model) see them as separate blocks.
      ...(parts.length > 1 ? { blocksJson: JSON.stringify(parts) } : {}),
      ...(origin === undefined ? {} : { origin }),
    } as SessionPrompt;
  }

  /// The prompt origin a submission's client metadata nests (v2 #3832): an
  /// `origin` field that names its `kind` is a prompt origin in the
  /// transcript contract's shape — the `skill_activation` variant a
  /// user-slash activation carries — and rides the turn request so the engine
  /// echoes it on the turn events. Mirrors the engine's own
  /// `prompt_origin_from_metadata`.
  private promptOriginFromClientMetadata(
    clientMetadata: ClientPromptMetadata | undefined,
  ): PromptOrigin | undefined {
    const origin = clientMetadata?.['origin'] as PromptOrigin | undefined;
    return origin !== undefined && typeof origin.kind === 'string' ? origin : undefined;
  }

  /// The deferred model-config failure the lenient loader promises (see
  /// `loadRuntimeConfigLenient`'s design note): a schema-invalid `[models]`
  /// entry stays in the loaded view, and this is the point it bites. An entry
  /// without a positive `max_context_size` cannot serve a turn — the engine
  /// would run a 0-token window and fail opaquely at the provider — so the
  /// submission is refused here with the schema's own actionable message.
  /// Entries the env synthesizer and the built-in defaults build always carry
  /// a window, so only a hand-written broken entry lands here.
  private assertSessionModelWindow(meta: NativeSessionMeta): void {
    const config = loadRuntimeConfigLenient(this.configPath);
    const alias = meta.model ?? config.defaultModel;
    if (alias === undefined) return;
    const entry = lookupModelAlias(config, alias);
    if (entry !== undefined && (effectiveModelAlias(entry).maxContextSize ?? 0) < 1) {
      throw new KimiError(
        ErrorCodes.CONFIG_INVALID,
        `Model "${alias}" must define a positive max_context_size in config.toml.`,
      );
    }
  }

  /**
   * Apply the prompt-derived metadata update (v2 `applyPromptMetadataUpdate`):
   * lastPrompt always, and the title only while it is still the untitled
   * default and not host-customized. Emits `session.meta.updated`.
   *
   * #3764 per-entry preference: when this entry's client supplied a
   * `displayText`, its sanitized form is the metadata the entry contributes —
   * it never mixes with `fallbackText` (the entry's sanitized content-derived
   * text), and an entry without one falls back to `fallbackText`. Every call
   * also appends one {@link NativePromptMetadataRecord} to
   * `meta.promptMetadata` so the displayText set judgment behind the
   * undo-label / fork-title derivations
   * ({@link displayTextForUndoOrForkLabel}) sees the entries in submission
   * order; an entry carrying neither a `displayText` nor metadata text is not
   * recorded. The record keeps the raw `displayText` even when it sanitizes
   * to empty: the flag records what the client provided, not what survived
   * redaction.
   */
  private applyPromptMetadata(
    meta: NativeSessionMeta,
    fallbackText: string | undefined,
    displayText?: string,
  ): void {
    const text = displayText !== undefined ? promptMetadataTextFromText(displayText) : fallbackText;
    if (displayText !== undefined || fallbackText !== undefined) {
      meta.promptMetadata.push({ text, hasDisplayText: displayText !== undefined, displayText });
    }
    if (text === undefined) return;
    const patch: { lastPrompt: string; title?: string } = { lastPrompt: text };
    if (!meta.isCustomTitle && isUntitledTitle(meta.title)) {
      patch.title = text.slice(0, MAX_TITLE_LENGTH);
      meta.title = patch.title;
      meta.titleKind = 'replaceable';
    }
    meta.lastPrompt = text;
    meta.updatedAt = Date.now();
    this.persistMeta(meta);
    this.receiveEvent({
      sessionId: meta.id,
      agentId: 'main',
      type: 'session.meta.updated',
      title: patch.title,
      patch: {
        title: patch.title,
        ...(patch.title !== undefined ? { isCustomTitle: false } : {}),
        lastPrompt: text,
      },
    });
  }

  /** The `agent.status.updated` snapshot emitted after a runtime-state change. */
  private emitStatusUpdated(meta: NativeSessionMeta): void {
    const usage = this.usageFromSnapshot(meta.usageJson);
    this.receiveEvent({
      sessionId: meta.id,
      agentId: 'main',
      type: 'agent.status.updated',
      model: meta.model,
      thinkingEffort: meta.thinkingEffort,
      permission: meta.permissionMode,
      planMode: meta.planMode,
      swarmMode: meta.swarmMode,
      towerMode: meta.towerMode,
      contextTokens: meta.contextTokens,
      maxContextTokens: meta.maxContextTokens,
      contextUsage:
        meta.maxContextTokens > 0 ? meta.contextTokens / meta.maxContextTokens : 0,
      ...(Object.keys(usage).length > 0 ? { usage } : {}),
    });
  }

  /** Map the engine history onto the SDK context shape (origins included). */
  private contextFromHistory(
    meta: NativeSessionMeta,
    history: readonly SessionPrompt[],
  ): AgentContextData {
    return {
      history: history.map((message, index) => {
        const raw = message as unknown as Record<string, unknown>;
        const rawRole = typeof raw['role'] === 'string' ? raw['role'] : 'user';
        const role: 'user' | 'assistant' | 'system' | 'tool' =
          rawRole === 'assistant' || rawRole === 'tool' || rawRole === 'system' ? rawRole : 'user';

        let toolCalls: ToolCall[] | undefined;
        const rawToolCalls = raw['tool_calls'] ?? raw['toolCalls'];
        if (Array.isArray(rawToolCalls)) {
          toolCalls = rawToolCalls.map((tc: unknown) => {
            const call = (tc && typeof tc === 'object' ? tc : {}) as Record<string, unknown>;
            const rawArgs = call['arguments'];
            const args =
              typeof rawArgs === 'string'
                ? rawArgs
                : rawArgs !== undefined && rawArgs !== null
                  ? JSON.stringify(rawArgs)
                  : null;
            return {
              type: 'function' as const,
              id: typeof call['id'] === 'string' ? call['id'] : '',
              name: typeof call['name'] === 'string' ? call['name'] : '',
              arguments: args,
            };
          });
        } else if (typeof raw['toolCallsJson'] === 'string') {
          try {
            const parsed = JSON.parse(raw['toolCallsJson']) as unknown;
            if (Array.isArray(parsed)) {
              toolCalls = parsed.map((tc: unknown) => {
                const call = (tc && typeof tc === 'object' ? tc : {}) as Record<string, unknown>;
                const rawArgs = call['arguments'];
                const args =
                  typeof rawArgs === 'string'
                    ? rawArgs
                    : rawArgs !== undefined && rawArgs !== null
                      ? JSON.stringify(rawArgs)
                      : null;
                return {
                  type: 'function' as const,
                  id: typeof call['id'] === 'string' ? call['id'] : '',
                  name: typeof call['name'] === 'string' ? call['name'] : '',
                  arguments: args,
                };
              });
            }
          } catch {}
        }

        const toolCallId =
          typeof raw['tool_call_id'] === 'string'
            ? raw['tool_call_id']
            : typeof raw['toolCallId'] === 'string'
              ? raw['toolCallId']
              : undefined;

        const isError = Boolean(raw['is_error'] ?? raw['isError']);

        const rawOrigin = raw['origin'] as PromptOrigin | undefined;
        const origin: PromptOrigin = rawOrigin ?? { kind: 'user' };

        return {
          id: `msg_${index}`,
          role,
          content: this.contextMessageContent(message),
          toolCalls: toolCalls ?? (role === 'assistant' ? [] : undefined),
          toolCallId,
          isError: isError || undefined,
          origin,
        };
      }),
      tokenCount: meta.contextTokens ?? 0,
    };
  }

  private contextMessageContent(
    message: SessionPrompt,
  ): ReadonlyArray<{ type: 'text'; text: string }> {
    const directBlocks = (message as { blocks?: unknown }).blocks;
    if (Array.isArray(directBlocks) && directBlocks.length > 0) {
      return directBlocks
        .filter(
          (block): block is { type: 'text'; text: string } =>
            isPlainObject(block) && block['type'] === 'text' && typeof block['text'] === 'string',
        )
        .map((block) => ({ type: 'text' as const, text: block.text }));
    }
    if (message.blocksJson !== undefined) {
      try {
        const blocks = JSON.parse(message.blocksJson) as unknown;
        if (Array.isArray(blocks) && blocks.length > 0) {
          return blocks
            .filter(
              (block): block is { type: 'text'; text: string } =>
                isPlainObject(block) &&
                block['type'] === 'text' &&
                typeof block['text'] === 'string',
            )
            .map((block) => ({ type: 'text' as const, text: block.text }));
        }
      } catch {
        // fall through to the plain content
      }
    }
    return [{ type: 'text', text: message.content ?? '' }];
  }

  /**
   * Persist the live engine history to `<sessionDir>/history.jsonl` so a
   * resume can replay it into a fresh handle (the transcript persistence the
   * native harness owns).
   */
  private async persistHistory(meta: NativeSessionMeta): Promise<void> {
    if (!meta.handle) return;
    try {
      const history = await meta.handle.getHistory();
      mkdirSync(meta.sessionDir, { recursive: true });
      writeFileSync(
        join(meta.sessionDir, 'history.jsonl'),
        history.map((message) => JSON.stringify(message)).join('\n') +
          (history.length > 0 ? '\n' : ''),
        'utf8',
      );
    } catch {
      // best-effort durability: a history write failure must not break the turn
    }
  }

  private readPersistedHistory(meta: NativeSessionMeta): SessionPrompt[] {
    try {
      const raw = readFileSync(join(meta.sessionDir, 'history.jsonl'), 'utf-8');
      return raw
        .split('\n')
        .filter((line) => line.trim().length > 0)
        .map((line) => JSON.parse(line) as SessionPrompt);
    } catch {
      return [];
    }
  }

  async ensureConfigFile(): Promise<void> {
    await ensureConfigFileScaffold(this.configPath);
  }

  async close(): Promise<void> {
    for (const meta of this.liveSessions.values()) {
      if (meta.handle) {
        await meta.handle.dispose().catch(() => {});
      }
      // Close each session sink while its meta is still in hand: the clear()
      // below drops the last reference that can reach it, and an unclosed sink
      // leaves whatever is still buffered on the floor.
      await this.detachSessionLog(meta);
    }
    this.liveSessions.clear();
    if (this.pluginStoreReady) {
      // Release the SQLite handle: Windows keeps a lock on an open database,
      // which blocks removing the data directory.
      const { closePluginStore } = await import('@moonshot-ai/kimi-agent/native');
      closePluginStore();
      this.pluginStoreReady = false;
    }
    await flushDiagnosticLogs();
  }
}

export function createKimiHarnessNative(options: SDKRpcClientNativeOptions): KimiHarness {
  const rpc = new SDKRpcClientNative(options);
  return new KimiHarness(rpc, {
    identity: rpc.identity,
    uiMode: options.uiMode,
    homeDir: rpc.homeDir,
    configPath: rpc.configPath,
    auth: rpc.auth,
    telemetry: rpc.telemetry,
    ensureConfigFile: () => rpc.ensureConfigFile(),
    onClose: () => rpc.close(),
    // The in-process core resolves its owner-scoped [image] limits from its
    // own config file (env var > config > built-in default).
    imageLimits:
      options.imageLimits ??
      new ImageLimits(process.env, {
        maxEdgePx: loadRuntimeConfigLenient(rpc.configPath).image?.maxEdgePx,
        readByteBudget: loadRuntimeConfigLenient(rpc.configPath).image?.readByteBudget,
      }),
    sessionStartedProperties: options.sessionStartedProperties,
  });
}

export {
  normalizeEngineTurnId,
  displayTextForUndoOrForkLabel,
  isExperimentalFlagEnabled,
  discoverSubagentsFromToolResult,
} from './sdk-client/helpers';
export type { SDKRpcClientNativeOptions, DiscoveredSubagent } from './sdk-client/helpers';
