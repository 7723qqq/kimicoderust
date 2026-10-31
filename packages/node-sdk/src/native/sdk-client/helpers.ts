import { existsSync, readFileSync, realpathSync } from 'node:fs';
import { isAbsolute, join, resolve } from 'node:path';

import type { EngineSessionHandle, SessionPrompt } from '@moonshot-ai/kimi-agent/session-handle';
import type { OAuthRefreshOutcome } from '@moonshot-ai/kimi-code-oauth';
import type { KimiErrorCode, TokenUsage, TurnEndReason } from '@moonshot-ai/protocol';
import { kimiErrorCodeSchema } from '@moonshot-ai/protocol';

import type { KimiAuthFacade } from '#/auth';
import { type KimiConfig } from '#/config-local';
import { ErrorCodes, KimiError } from '#/error-protocol';
import type { ImageLimits } from '#/image-limits';
import type { SessionLogHandle } from '#/logging';
import { type SessionPromptRpcInput } from '#/rpc';
import type {
  AgentMeta,
  ClientPromptMetadata,
  PromptOriginMetadataEntry,
  SkillSource,
  SwarmModeTrigger,
  ExperimentalFeatureState,
  GoalSnapshot,
  GoalStatus,
  KimiHostIdentity,
  PermissionMode,
  PluginInfo,
  PromptPart,
  TelemetryClient,
  TelemetryProperties,
  ExperimentalFlagSource,
} from '#/types';

import { resolveModelContextWindow, type JsNativeLlmConfig } from '../native-llm-resolver';

/**
 * Map the engine's snake_case usage payload onto the protocol's camelCase
 * {@link TokenUsage}. Returns `undefined` when the engine reported none.
 */
export function toTokenUsage(raw: unknown): TokenUsage | undefined {
  if (raw === null || typeof raw !== 'object') return undefined;
  const usage = raw as Record<string, unknown>;
  const num = (key: string): number => (typeof usage[key] === 'number' ? usage[key] : 0);
  return {
    inputOther: num('input_tokens'),
    output: num('output_tokens'),
    inputCacheRead: num('input_cache_read'),
    inputCacheCreation: num('input_cache_creation'),
  };
}

/**
 * Normalize an engine turn id to the numeric id the protocol events carry.
 *
 * The engine mints turn ids in two string shapes — `turn-<n>` on the event lane
 * (`EngineEvent::TurnStarted`) and `subturn-<rand>` on the subagent lane
 * (`subagent/manager.rs`) — and as a plain number on the turn-lifecycle lane
 * (`turn_events::TurnEvent`, `turn_id: u64`). `Number.parseInt` only reads a
 * *leading* run of digits, so `turn-3` parsed to `NaN` and `NaN || 0` mapped
 * every such turn onto 0 — the value `meta.currentTurnId` also starts at, so
 * the collapse was invisible.
 *
 * Take the *trailing* digits instead, which is the normalization the transcript
 * fold already applies to the same ids (`server/transcript/project.rs`:
 * `turn_key` keeps the digits after the last non-digit and re-keys them as
 * `t<number>`, so `turn-3`, `t3` and `subturn-3` are all one turn). Keeping only
 * the number is what makes the prefix irrelevant here: `subturn-7` is turn 7,
 * not a second namespace.
 *
 * An id with no trailing digits, or no id at all, degrades to the documented
 * fallback `0` — the same "no turn known" value `meta.currentTurnId` starts at,
 * and a defined one rather than `NaN`. Never returns `NaN`.
 */
export function normalizeEngineTurnId(raw: unknown): number {
  if (typeof raw === 'number') return Number.isFinite(raw) ? raw : 0;
  if (typeof raw !== 'string') return 0;
  const digits = /(\d+)$/.exec(raw)?.[1];
  if (digits === undefined) return 0;
  const parsed = Number.parseInt(digits, 10);
  return Number.isNaN(parsed) ? 0 : parsed;
}

/**
 * Map the Rust engine's turn stop reason onto the protocol's closed
 * {@link TurnEndReason} set. The engine emits a wider vocabulary (aborted /
 * max_steps / length / tool_calls / …); forwarding those verbatim broke
 * consumers that switch on the four protocol values.
 */
export function toTurnEndReason(raw: unknown): TurnEndReason {
  const value = (typeof raw === 'string' ? raw : 'completed').toLowerCase();
  if (
    value === 'cancelled' ||
    value === 'canceled' ||
    value === 'aborted' ||
    value === 'interrupted'
  ) {
    return 'cancelled';
  }
  if (
    value === 'failed' ||
    value === 'error' ||
    value === 'max_steps' ||
    value === 'max_tokens' ||
    value === 'length'
  ) {
    return 'failed';
  }
  if (value === 'blocked') {
    return 'blocked';
  }
  return 'completed';
}

/**
 * The engine reports error codes as free-form strings; the protocol union is
 * closed, so an unrecognized code degrades to `internal` rather than dropping
 * the message that came with it.
 */
export function toKimiErrorCode(raw: unknown): KimiErrorCode {
  const parsed = kimiErrorCodeSchema.safeParse(raw);
  return parsed.success ? parsed.data : 'internal';
}

/**
 * The native harness's TS-side goal state. The Rust engine pulls a fresh goal
 * snapshot per turn through the `goal` callback (snake_case wire), so this is
 * the authority for the goal lifecycle; budgets are not modelled natively yet,
 * so the budget report is all-null / not-reached.
 */
export interface NativeGoalState {
  goalId: string;
  objective: string;
  status: GoalStatus;
  createdAt: number;
  updatedAt: number;
  turnsUsed: number;
  tokensUsed: number;
  inputTokensUsed: number;
  outputTokensUsed: number;
}

export function toGoalSnapshot(goal: NativeGoalState): GoalSnapshot {
  return {
    goalId: goal.goalId,
    objective: goal.objective,
    status: goal.status,
    turnsUsed: goal.turnsUsed,
    tokensUsed: goal.tokensUsed,
    inputTokensUsed: goal.inputTokensUsed,
    outputTokensUsed: goal.outputTokensUsed,
    wallClockMs: Date.now() - goal.createdAt,
    budget: {
      tokenBudget: null,
      turnBudget: null,
      wallClockBudgetMs: null,
      remainingTokens: null,
      remainingTurns: null,
      remainingWallClockMs: null,
      tokenBudgetReached: false,
      turnBudgetReached: false,
      wallClockBudgetReached: false,
      overBudget: false,
      inputTokensUsed: goal.inputTokensUsed,
      outputTokensUsed: goal.outputTokensUsed,
    },
    createdAt: goal.createdAt,
    updatedAt: goal.updatedAt,
  };
}

/**
 * The runtime agent state a fresh or resumed session starts with, derived from
 * the on-disk config. The setters that change these mid-session need an
 * engine-handle rebuild (the policy snapshot and native LLM are baked in at
 * createSession), so they land with the persistence work; until then the
 * getters read these config-derived defaults.
 */
export function initialRuntimeState(config: KimiConfig, model: string | undefined) {
  return {
    model,
    thinkingEffort:
      config.thinking?.enabled === false ? 'off' : (config.thinking?.effort ?? 'medium'),
    permissionMode: (config.yolo === true
      ? 'yolo'
      : config.defaultPermissionMode === 'auto'
        ? 'auto'
        : 'manual') as PermissionMode,
    planMode: (config.defaultPermissionMode as string | undefined) === 'plan',
    swarmMode: false,
    swarmModeTrigger: 'manual' as const,
    towerMode: false,
    maxContextTokens: resolveModelContextWindow(config, config.defaultModel),
    contextTokens: 0,
    usageJson: undefined,
    goal: null,
    currentTurnId: 0,
  };
}

/**
 * Apply a session's live thinking override to the config-resolved native LLM.
 * setThinking mutates meta and rebuilds the handle, so the rebuilt pipeline
 * must carry the session's choice, not the config default. The thinking-budget
 * re-derivation mirrors native-llm-resolver's.
 *
 * The model is deliberately NOT overridden here: `meta.model` is a `[models]`
 * alias (`ollama/deepseek-v4.1-flash`), while `llm.model` is the wire model
 * that alias resolves to (`deepseek-v4.1-flash`). buildHandle already resolves
 * the LLM from `meta.model`, so copying the alias over `llm.model` sent the
 * alias to the provider — every alias whose wire model differs from its own
 * name came back as "model not found".
 */
export function applySessionLlmOverrides(
  llm: JsNativeLlmConfig,
  meta: { thinkingEffort: string },
): JsNativeLlmConfig {
  const out: JsNativeLlmConfig = { ...llm };
  const effort = meta.thinkingEffort;
  const thinkingOn = effort !== '' && effort !== 'off' && effort !== 'none';
  if (out.protocol === 'anthropic') {
    out.reasoningEffort = undefined;
    if (!thinkingOn) out.thinkingBudget = undefined;
    else if (effort === 'low') out.thinkingBudget = 1024;
    else if (effort === 'medium') out.thinkingBudget = 4096;
    else if (effort === 'high' || effort === 'on') out.thinkingBudget = 32000;
    else {
      const parsed = Number.parseInt(effort, 10);
      out.thinkingBudget = !Number.isNaN(parsed) && parsed > 0 ? parsed : 32000;
    }
  } else {
    out.thinkingBudget = undefined;
    out.reasoningEffort = thinkingOn ? effort : undefined;
  }
  // `[thinking] keep` is only injected while thinking is on (env-vars.md).
  out.thinkingKeep = thinkingOn ? llm.thinkingKeep : undefined;
  return out;
}

export function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/**
 * The api key a previous import from `url` parked on its providers' `source`
 * blob. The URL is the stable identity of "the same registry" — the key
 * commonly rotates between imports — so a re-import that omits `--api-key`
 * reuses this one instead of failing the fetch.
 */
export function registryKeyFromExisting(
  providers: Record<string, unknown>,
  url: string,
): string | undefined {
  for (const provider of Object.values(providers)) {
    if (!isPlainObject(provider)) continue;
    const source = provider['source'];
    if (isPlainObject(source) && source['kind'] === 'apiJson' && source['url'] === url) {
      const key = source['apiKey'];
      if (typeof key === 'string' && key.length > 0) return key;
    }
  }
  return undefined;
}

export function truncateUpstreamMessage(error: unknown, limit = 300): string {
  const text = error instanceof Error ? error.message : String(error);
  return text.length > limit ? `${text.slice(0, limit)}…` : text;
}

/** `undefined` clears the key rather than persisting an explicit null. */
export function assignOrDelete(target: Record<string, unknown>, key: string, value: unknown): void {
  if (value === undefined) {
    delete target[key];
  } else {
    target[key] = value;
  }
}

/**
 * The `liveShellCommands` key for one `!` shell command. `undefined` when the
 * caller passed no `commandId` — without one there is nothing to cancel by, so
 * the handle is not published.
 */
export function shellCommandKey(
  sessionId: string,
  commandId: string | undefined,
): string | undefined {
  return commandId === undefined ? undefined : `${sessionId}\u0000${commandId}`;
}

/**
 * The history cut index that removes the last `count` user turns: walking from
 * the end, the Nth-from-last user message starts the earliest turn to drop, so
 * everything from it onward is undone. Fewer than `count` user turns clears the
 * whole history.
 */
export function truncateHistoryByTurns(history: readonly SessionPrompt[], count: number): number {
  if (count <= 0) return history.length;
  let seen = 0;
  for (let i = history.length - 1; i >= 0; i--) {
    if (history[i]?.role === 'user') {
      seen += 1;
      if (seen === count) return i;
    }
  }
  return 0;
}

/**
 * The history cut that retains user turns 0..turnIndex: the (index+1)-th user
 * message from the start ends the retained window, so everything from it
 * onward is dropped.
 */
export function retainThroughTurn(history: readonly SessionPrompt[], turnIndex: number): number {
  let seen = 0;
  for (let i = 0; i < history.length; i++) {
    if (history[i]?.role === 'user') {
      if (seen === turnIndex) return i + 1;
      seen += 1;
    }
  }
  return history.length;
}

/**
 * v2 `config.set` merge semantics for one domain: plain objects merge
 * recursively, arrays and scalars replace. An explicit `undefined` inside the
 * patch leaves the stored value untouched (mirrors the per-domain undefined
 * skip), so a patch can never silently clear a section.
 */
export function deepMergeConfigValue(current: unknown, patch: unknown): unknown {
  if (isPlainObject(current) && isPlainObject(patch)) {
    const out: Record<string, unknown> = { ...current };
    for (const [key, value] of Object.entries(patch)) {
      if (value === undefined) continue;
      out[key] = deepMergeConfigValue(current[key], value);
    }
    return out;
  }
  return patch;
}

/** The SDK normalizes returned paths to forward slashes (pathe semantics). */
export function posixPath(path: string): string {
  return path.replaceAll('\\', '/');
}

/**
 * Expand a plugin command body against its arguments (v2
 * `expandCommandArguments`): `$ARGUMENTS` is replaced in place, and a body
 * without the placeholder gets the args appended as an `ARGUMENTS:` trailer so
 * they are never dropped.
 */
export function expandCommandArguments(body: string, args: string): string {
  const replaced = body.replaceAll('$ARGUMENTS', args);
  if (!body.includes('$ARGUMENTS') && args.length > 0) {
    return `${replaced}\n\nARGUMENTS: ${args}`;
  }
  return replaced;
}

/** v1's `requiredWorkDir`: reject blank and normalize to the canonical spelling. */
export function normalizeRequiredWorkDir(operation: string, workDir: unknown): string {
  if (typeof workDir !== 'string' || workDir.trim() === '') {
    throw new KimiError(ErrorCodes.REQUEST_WORK_DIR_REQUIRED, `${operation} requires workDir`);
  }
  return posixPath(resolve(workDir));
}

/**
 * v2 keys a workspace trust record by the canonical root
 * (`trustRecord.trustKey`), so one directory matches however the caller spelled
 * it. Comparing the raw strings instead made a trusted workspace read as
 * untrusted — silently dropping the roots its `local.toml` asks for — whenever
 * the two spellings differed in separators or drive case.
 *
 * The key is the *real* path, not a normalized string: a symlink or junction
 * spelled one way today and another tomorrow names the same directory, and a
 * junction retargeted later would otherwise keep the trust its old target had.
 * `realpathSync.native` also collapses Windows' 8.3 short names, which are a
 * different string for the same directory.
 *
 * A path that cannot be resolved falls back to its absolute normalized form:
 * a workspace deleted after being trusted still has to match its stored entry
 * (and a missing path is never a *different* directory we would trust instead).
 */
export function workspaceTrustKey(root: string): string {
  const absolute = resolve(root);
  let real: string;
  try {
    real = realpathSync.native(absolute);
  } catch {
    real = absolute;
  }
  const canonical = posixPath(real).replace(/\/+$/, '');
  return process.platform === 'win32' ? canonical.toLowerCase() : canonical;
}

/**
 * Whether an id is safe to use as one path segment under the sessions root.
 * Session ids are `session_<uuid>` or client-chosen names that become
 * directory names, so anything that could escape the root — or be silently
 * rewritten by the platform — is rejected before the first `join`:
 *
 * - separators and control characters are direct mangling vectors;
 * - `:` carries Windows meaning: `C:foo` is drive-relative (isAbsolute says
 *   no, the OS still honors the drive) and `foo:bar` names an NTFS
 *   alternate data stream instead of a directory;
 * - Win32 strips trailing dots and spaces per path component, so `.. `
 *   resolves back to `..` and `evil.` aliases `evil` — hence the all-dots
 *   and trailing-dot/space checks that exact `.`/`..` matching misses;
 * - absolute roots (`C:\x`, `\\server`, `/x`) are not segments at all.
 *
 * Deliberately a blacklist, not a `[A-Za-z0-9._-]` whitelist: ids are
 * persisted directory names, and an existing session with a non-ASCII id
 * must stay resumable, renamable, and deletable.
 */
export function isSinglePathSegment(id: string): boolean {
  if (id.length === 0) return false;
  if (id.includes('/') || id.includes('\\')) return false;
  if (id.includes(':')) return false;
  // Code-point scan rather than a regex literal: `no-control-regex` flags
  // `[\u0000-\u001f]` patterns, and this loop reads as the intent.
  for (const ch of id) {
    const code = ch.codePointAt(0) ?? 0;
    if (code < 0x20 || code === 0x7f) return false;
  }
  if (/^\.+$/.test(id)) return false;
  if (id.endsWith('.') || id.endsWith(' ')) return false;
  return !isAbsolute(id);
}

/**
 * Reject an id that would escape the sessions root once joined into a path.
 * Every RPC that turns a client-supplied session id into a path segment —
 * create, resume, rename, delete, export — funnels through here: the id
 * becomes a directory that is read, written, zipped, or recursively removed,
 * so a separator, a `..` segment, or an absolute root must fail before any
 * filesystem call rather than after it.
 */
export function assertSessionIdSegment(id: string): void {
  if (!isSinglePathSegment(id)) {
    throw new KimiError(ErrorCodes.SESSION_ID_INVALID, `invalid session id "${id}"`, {
      details: { sessionId: id },
    });
  }
}

export const MAX_TITLE_LENGTH = 200;
export const MAX_LAST_PROMPT_LENGTH = 4000;

/**
 * The prompt-derived title/lastPrompt sanitizer, ported byte-identically from
 * the retired engine's `promptMetadataText` so prompt metadata redacts the
 * same credential shapes the engine used to.
 */
export function promptMetadataTextFromText(text: string): string | undefined {
  const sanitized = text
    .replaceAll(
      /-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?-----END [^-]*PRIVATE KEY-----/gi,
      '[redacted]',
    )
    .replaceAll(/\b(authorization)\s*:\s*bearer\s+\S+/gi, '$1: Bearer [redacted]')
    .replaceAll(
      /\b(api[_-]?key|token|secret|password|passwd|pwd)\b\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)/gi,
      '$1=[redacted]',
    )
    .replaceAll(/\bsk-[A-Za-z0-9_-]{12,}\b/g, '[redacted]')
    .replaceAll(/\b[A-Za-z0-9][A-Za-z0-9+/=_-]{39,}\b/g, '[redacted]')
    .replaceAll(/\p{Cc}+/gu, ' ')
    .replaceAll(/\s+/g, ' ')
    .trim();

  if (sanitized.length === 0) return undefined;
  return sanitized.slice(0, MAX_LAST_PROMPT_LENGTH);
}

export function promptMetadataTextFromPrompt(
  input: SessionPromptRpcInput['input'],
): string | undefined {
  const texts: string[] = [];
  for (const part of input) {
    if (part.type === 'text') texts.push(part.text);
    else if (part.type === 'image_url') texts.push('[image]');
    else texts.push('[video]');
  }
  return promptMetadataTextFromText(texts.join('\n'));
}

export function isUntitledTitle(title: string): boolean {
  return title.trim().length === 0 || title === 'New Session';
}

/**
 * One prompt-metadata record per submitted entry (prompt, steer, or skill
 * activation, v2 #3764), appended by {@link SDKRpcClientNative.applyPromptMetadata}
 * in submission order. `displayText` is the raw client string; `text` is what
 * the entry contributed to the session metadata — the sanitized `displayText`
 * when the caller supplied one, else the sanitized content-derived text
 * (`undefined` when that sanitizes to empty).
 */
/**
 * One `/skill:` activation as the engine resolved it: the rendered prompt plus
 * the provenance v2's `SkillActivationOrigin` carries, which the host was
 * previously inventing from a path guess (`source="project"` for every skill,
 * whether it was a builtin or an `extra_skill_dirs` entry).
 */
export interface ResolvedSkillPrompt {
  readonly text: string;
  /** The catalog's name for it — not necessarily the requested one. */
  readonly name: string;
  readonly path?: string;
  readonly source?: SkillSource;
  readonly skillType?: string;
}

/** The `SkillSource` vocabulary the engine's scan emits. */
export const SKILL_SOURCES: ReadonlySet<string> = new Set<SkillSource>([
  'project',
  'user',
  'extra',
  'builtin',
]);

/**
 * The wire shape of a prompt origin's `clientMetadata`: v2 carries an **array**
 * of per-submission entries, each with a `display_text`
 * (`contextMemory/types.ts:20`, consumed by `promptMetadataTextFromContentParts`).
 * The host's own `ClientPromptMetadata` is a single object with `displayText`,
 * and every consumer of the folded value — `transcript`'s
 * `projectTranscriptUserOrigin` and its `TranscriptUserOrigin` /
 * `schema.ts` types — is already written against the array. Emitting the object
 * therefore dropped every origin's metadata on the floor, silently.
 */
export function originClientMetadata(
  clientMetadata: ClientPromptMetadata | undefined,
): PromptOriginMetadataEntry[] | undefined {
  const displayText = clientMetadata?.displayText;
  return typeof displayText === 'string' && displayText.length > 0
    ? [{ display_text: displayText }]
    : undefined;
}

export interface NativePromptMetadataRecord {
  readonly text: string | undefined;
  readonly hasDisplayText: boolean;
  readonly displayText?: string;
}

/**
 * The #3764 displayText set judgment behind upstream's undo-label
 * (`undoService.reconcileLastPrompt`) and fork-title
 * (`forkTurnSlice.promptMetadataFromTurnRecord`) derivations: a derivation
 * may use the client displayTexts only when EVERY prompt entry provides one —
 * a single entry without `displayText` falls the whole derivation back to the
 * existing text-derived metadata, never a per-entry mix.
 *
 * The native SDK has no undo-label or fork-title decision points yet
 * (`undoHistory` truncates history without relabeling, `forkSession` takes its
 * title from the caller, `generateSessionTitle` delegates to the engine's own
 * title source), so this exported helper is the wiring seam for those
 * derivations: pass the session's `NativeSessionMeta.promptMetadata` records
 * in submission order and use the returned sanitized join. `undefined` has two
 * distinct causes, mirroring upstream (v2 `promptMetadataText.ts` +
 * `applyPromptMetadataUpdate`): a record without `displayText` (or an empty
 * set) falls the derivation back to the existing text-derived metadata, while
 * an all-`displayText` set whose sanitized join is empty (reachable with e.g.
 * every entry `displayText: ''`) means upstream applies NO metadata update at
 * all — a consumer must not fall back to text derivation there. The two are
 * told apart with `records.every((record) => record.hasDisplayText)` on the
 * records the caller already holds. Exported for that consumer and for tests;
 * `applyPromptMetadata` produces the records.
 */
export function displayTextForUndoOrForkLabel(
  records: readonly NativePromptMetadataRecord[],
): string | undefined {
  if (records.length === 0 || records.some((record) => !record.hasDisplayText)) {
    return undefined;
  }
  return promptMetadataTextFromText(
    records
      .map((record) => record.displayText)
      .filter((text): text is string => text !== undefined)
      .join('\n'),
  );
}

/** Byte-identical with the v1 import-context guidance text. */
export const IMPORT_CONTEXT_GUIDANCE =
  'This is a prior conversation history that may be relevant to the current session. ' +
  'Please review this context and use it to inform your responses.';

/** Byte-identical with v1's `escapeXml` (& < > "). */
export function escapeXml(input: string): string {
  return input
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');
}

/** Byte-identical with v1's `escapeXmlAttr` (& " only). */
export function escapeXmlAttr(input: string): string {
  return input.replaceAll('&', '&amp;').replaceAll('"', '&quot;');
}

/**
 * The exact user message v1's importContext appended, including its
 * rejections: blank content (`import_content_empty`) and blank source
 * (`import_source_empty`) fail with v1's `request.invalid` shapes before any
 * token math runs.
 */
export function buildImportContextParts(content: string, source: string): PromptPart[] {
  if (content.trim().length === 0) {
    throw new KimiError(ErrorCodes.REQUEST_INVALID, 'Imported context cannot be empty', {
      details: { reason: 'import_content_empty' },
    });
  }
  const normalizedSource = source.trim();
  if (normalizedSource.length === 0) {
    throw new KimiError(ErrorCodes.REQUEST_INVALID, 'Imported context source cannot be empty', {
      details: { reason: 'import_source_empty' },
    });
  }
  return [
    {
      type: 'text',
      text:
        `<system>The user has imported context from ${escapeXml(normalizedSource)}. ` +
        `${IMPORT_CONTEXT_GUIDANCE}</system>`,
    },
    {
      type: 'text',
      text:
        `<imported_context source="${escapeXmlAttr(normalizedSource)}">\n` +
        `${content}\n</imported_context>`,
    },
  ];
}

/** v1's overflow gate: the import estimate plus the current context must fit the model window. */
export function assertImportFits(
  messageTokens: number,
  currentTokenCount: number,
  maxContextTokens: number,
): void {
  const totalTokenCount = currentTokenCount + messageTokens;
  if (maxContextTokens > 0 && totalTokenCount > maxContextTokens) {
    throw new KimiError(
      ErrorCodes.CONTEXT_OVERFLOW,
      'Imported content is too large for the current model context ' +
        `(~${String(messageTokens)} import tokens + ~${String(currentTokenCount)} existing ` +
        `= ~${String(totalTokenCount)} total > ${String(maxContextTokens)} token limit). ` +
        'Please import a smaller file or session.',
      {
        details: {
          reason: 'import_context_overflow',
          importTokenCount: messageTokens,
          currentTokenCount,
          totalTokenCount,
          maxContextTokens,
        },
      },
    );
  }
}

/**
 * The experimental flag registry the engine used to own. The native harness
 * serves the same metadata over `getExperimentalFeatures`; precedence per
 * flag is env > `[experimental]` config > master env > the flag's default.
 */
export interface NativeExperimentalFlag {
  readonly id: string;
  readonly title: string;
  readonly description: string;
  readonly env: string;
  readonly defaultEnabled: boolean;
  readonly surface: string;
}

export const NATIVE_EXPERIMENTAL_FLAGS: readonly NativeExperimentalFlag[] = [
  {
    id: 'tool_select',
    title: 'Tool select (progressive tool disclosure)',
    description:
      'Keep MCP tool schemas out of the immutable top-level tools[]; the model loads them on demand via the select_tools tool. Only takes effect on models whose capability catalog declares dynamically loaded tools.',
    env: 'KIMI_CODE_EXPERIMENTAL_TOOL_SELECT',
    defaultEnabled: true,
    surface: 'core',
  },
  {
    id: 'notify_user',
    title: 'Updates panel (NotifyUser tool events)',
    description:
      'Show an experimental Updates panel with paginated progress messages from the main agent and subagents via the NotifyUser tool.',
    env: 'KIMI_CODE_EXPERIMENTAL_NOTIFY_USER',
    defaultEnabled: false,
    surface: 'core',
  },
  {
    id: 'tower',
    title: 'Tower mode',
    description:
      'Enable tower mode: coordinate multiple agents on a shared objective, toggled with the /tower command.',
    // Must stay identical to the engine's `TOWER_ENV_SWITCH`
    // (`packages/kimi-agent/src/tools/tower/paths.rs`): the host flag and the
    // engine's advertised tool table read the same switch.
    env: 'KIMI_CODE_EXPERIMENTAL_TOWER',
    defaultEnabled: false,
    surface: 'both',
  },
  {
    id: 'xunfei_coding_plan',
    title: 'Astron (Xunfei coding plan)',
    description:
      'Show the Astron provider settings in /settings and enable the Astron login flow. The astron entry under [providers] stays functional regardless of this flag.',
    env: 'KIMI_CODE_EXPERIMENTAL_XUNFEI_CODING_PLAN',
    defaultEnabled: false,
    surface: 'both',
  },
];

/**
 * Whether one experimental flag is enabled for this config — the registry's
 * precedence (env > `[experimental]` > master env > default). Engine-param
 * resolvers read this instead of duplicating the flag logic.
 */
export function isExperimentalFlagEnabled(config: KimiConfig, flagId: string): boolean {
  return resolveExperimentalFeatures(config).find((flag) => flag.id === flagId)?.enabled === true;
}

export function resolveExperimentalFeatures(
  config: KimiConfig,
): readonly ExperimentalFeatureState[] {
  const masterEnv = process.env['KIMI_CODE_EXPERIMENTAL_FLAG'];
  return NATIVE_EXPERIMENTAL_FLAGS.map((flag) => {
    const envValue = process.env[flag.env];
    const configValue = config.experimental?.[flag.id];
    let enabled = flag.defaultEnabled;
    let source: ExperimentalFlagSource = 'default';
    if (envValue !== undefined) {
      enabled = envValue !== '0' && envValue.toLowerCase() !== 'false';
      source = 'env';
    } else if (typeof configValue === 'boolean') {
      enabled = configValue;
      source = 'config';
    } else if (
      masterEnv !== undefined &&
      masterEnv !== '0' &&
      masterEnv.toLowerCase() !== 'false'
    ) {
      // A master switch of 0/false means "no force-enable"; flags keep their
      // own defaults rather than being disabled wholesale.
      enabled = true;
      source = 'master-env';
    }
    return {
      id: flag.id,
      title: flag.title,
      description: flag.description,
      env: flag.env,
      defaultEnabled: flag.defaultEnabled,
      enabled,
      source,
      surface: flag.surface,
      ...(configValue !== undefined ? { configValue } : {}),
    };
  });
}

/** A stored mcp.json entry as read from disk (the transport tag is optional). */
export interface StoredMcpServerConfig {
  transport?: 'stdio' | 'http' | 'sse';
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  headers?: Record<string, string>;
  auth?: 'oauth';
  bearerTokenEnvVar?: string;
  enabled?: boolean;
  name?: string;
  /** Keep this server's tools out of the top-level list; load them via `select_tools`. */
  deferred?: boolean;
}

export interface SDKRpcClientNativeOptions {
  readonly homeDir?: string | undefined;
  readonly configPath?: string | undefined;
  /**
   * Data directory of the app-scope engine store (`<dir>/sessions.db`), the
   * SQLite database the plugin registry lives in. Defaults to
   * `<homeDir>/agent`, which is where the CLI hosts the native server
   * (`rust-server-runner` passes `--data-dir <home>/agent`); a host that runs
   * the server against a different `--data-dir` sets this so the CLI and the
   * server read one install state instead of two that silently disagree.
   */
  readonly engineDataDir?: string | undefined;
  readonly identity?: KimiHostIdentity | undefined;
  readonly auth?: KimiAuthFacade | undefined;
  readonly onOAuthRefresh?: ((outcome: OAuthRefreshOutcome) => void) | undefined;
  readonly telemetry?: TelemetryClient | undefined;
  readonly uiMode?: string | undefined;
  readonly sessionStartedProperties?: TelemetryProperties | undefined;
  readonly imageLimits?: ImageLimits | undefined;
  readonly skillDirs?: readonly string[] | undefined;
}

export interface NativeSessionMeta {
  readonly id: string;
  readonly workDir: string;
  readonly sessionDir: string;
  readonly createdAt: number;
  updatedAt: number;
  title: string;
  isCustomTitle: boolean;
  /** Prompt-derived easy title marker (v2 `titleKind: 'replaceable'`). */
  titleKind: 'default' | 'replaceable' | 'custom' | 'generated';
  lastPrompt: string | undefined;
  busy: boolean;
  messageCount: number;
  /** Last turn id seen from the engine; kept on meta so it survives a rebuild. */
  currentTurnId: number;
  /**
   * Per-entry prompt-metadata records in submission order (#3764), one per
   * prompt / steer / skill activation that carried metadata. Feeds the
   * displayText set judgment for undo-label / fork-title derivations
   * (`displayTextForUndoOrForkLabel`). In-memory only: the derivations it
   * serves run live, and the durable lastPrompt/title stay on
   * {@link PersistedSessionMeta}.
   */
  promptMetadata: NativePromptMetadataRecord[];
  /** User-layer session metadata (v2 `session.custom`), merged by updateSessionMetadata. */
  custom: Record<string, unknown>;
  /**
   * User-configured ask rules the user approved "for this session".
   *
   * v2 keeps the same list as `sessionApprovalRulePatterns` and promotes a
   * pattern into it only when the approval response says `scope: session`
   * (permissionRulesOps.ts). Approving one of these remembers *that rule* — a
   * pattern the user already wrote — rather than the whole tool: a session-wide
   * "allow Bash" would permit every later Bash call, including the dangerous
   * ones the rule was written to catch.
   *
   * In memory only, like `promptMetadata`: the patterns describe decisions made
   * in this session, and re-deriving them from a config file that may have
   * changed would attribute a decision the user never made.
   */
  sessionApprovals: string[];
  /**
   * Workspace-level additional directories added via addAdditionalDir, or passed
   * by the host at create/resume. These are the caller's own choices, so they
   * are persisted with the session.
   */
  additionalDirs: string[];
  /**
   * Roots the *project* asked for through `workspace.additional_dir`, resolved
   * while the workspace is trusted (v2 `WorkspaceDirsService`'s `fileDirs`, as
   * opposed to its `ephemeralDirs`).
   *
   * Deliberately never persisted: the project's file is re-read on every
   * create/resume/rebuild, so revoking trust withdraws these roots instead of
   * leaving a session that still carries what the file said while it was
   * trusted. Keeping the two lists apart is also what keeps an untrusted
   * checkout from smuggling its file's roots in as "caller-requested" ones.
   */
  projectAdditionalDirs: string[];
  /**
   * Headless session (`kimi -p`): rides the policy snapshot to the engine,
   * which skips its `DangerousCommandAsk` policy for the session's turns.
   */
  nonInteractive: boolean;
  // Runtime agent state (re-derived from config on resume, not persisted): the
  // getters (getStatus / getUsage) read these; the setters that would change
  // them mid-session need an engine-handle rebuild and land with persistence.
  model: string | undefined;
  thinkingEffort: string;
  permissionMode: PermissionMode;
  planMode: boolean;
  swarmMode: boolean;
  /**
   * Why the host opened swarm mode. `task` and `tool` swarms close themselves
   * at turn end, so the engine needs the distinction to decide whether the
   * turn's end is a mode change; `manual` survives it. Defaults to `manual`,
   * the only trigger v2's profile route uses.
   */
  swarmModeTrigger: SwarmModeTrigger;
  towerMode: boolean;
  maxContextTokens: number;
  contextTokens: number;
  /**
   * The engine's `UsageStatus` snapshot (`{ byModel, total, currentTurn }`),
   * cached for sync status events and persisted across handle builds. The
   * engine owns the fold; the host never re-derives it.
   */
  usageJson?: string | undefined;
  goal: NativeGoalState | null;
  /** The active plan document handle (plan mode); content lives in the plan file. */
  plan: { id: string; content: string; path: string } | undefined;
  /** Source session id when this session was forked. */
  forkedFrom: string | undefined;
  /** Main-agent profile bound at create (`--agent`); undefined = engine default. */
  agentProfile?: string;
  /** Raw `--agent-file` paths; re-read on every handle build. */
  agentFiles?: readonly string[];
  activeAgentId?: string;
  handle?: EngineSessionHandle;
  /**
   * This session's own log sink. Attached when the session becomes live and
   * closed when it stops being live, so `<sessionDir>/logs/kimi-code.log`
   * exists for exactly as long as the session does.
   */
  logHandle?: SessionLogHandle;
  agents?: Record<string, AgentMeta>;
}

/**
 * The Rust `TaskRunner` entry wire (`storage/task_runner.rs:entry_wire`) —
 * snake_case-free already, the v2 task-domain shape.
 */
export interface EngineTaskWireEntry {
  taskId: string;
  description: string;
  status: string;
  startedAt: number;
  endedAt?: number;
  stopReason?: string;
  output?: string;
}

/**
 * The durable, handle-free slice of {@link NativeSessionMeta}, written to
 * `<sessionDir>/session-meta.json` so a rename / metadata update / add-dir
 * survives a resume within the same home. Full transcript persistence is a
 * separate concern; this only carries the session's own bookkeeping.
 */
export interface PersistedSessionMeta {
  id: string;
  workDir: string;
  createdAt: number;
  updatedAt: number;
  title: string;
  isCustomTitle: boolean;
  lastPrompt?: string | undefined;
  custom: Record<string, unknown>;
  additionalDirs: string[];
  model?: string | undefined;
  thinkingEffort?: string | undefined;
  permissionMode?: PermissionMode | undefined;
  planMode?: boolean | undefined;
  plan?: { id: string; content: string; path: string } | undefined;
  goal?: NativeGoalState | null | undefined;
  forkedFrom?: string | undefined;
  contextTokens?: number | undefined;
  /** The engine's opaque `UsageStatus` snapshot, restored into the next handle. */
  usageJson?: string | undefined;
  agents?: Record<string, AgentMeta> | undefined;
  /** Headless session (upstream `nonInteractive`): skips the engine's dangerous-command ask policy. */
  nonInteractive?: boolean | undefined;
  /** Main-agent profile bound at first create (`--agent`); restored on resume. */
  agentProfile?: string | undefined;
  /** Raw `--agent-file` paths; re-read on every handle build. */
  agentFiles?: readonly string[] | undefined;
}

export const DEFAULT_INIT_PROMPT = `You are a software engineering expert with many years of programming experience. Please explore the current project directory to understand the project's architecture and main details.

Task requirements:
1. Analyze the project structure and identify key configuration files (such as pyproject.toml, package.json, Cargo.toml, etc.).
2. Understand the project's technology stack, build process and runtime architecture.
3. Identify how the code is organized and main module divisions.
4. Discover project-specific development conventions, testing strategies, and deployment processes.

After the exploration, do a thorough summary of your findings and write it to the \`AGENTS.md\` file in the project root, replacing the file's previous content. If the file already exists, read it first and carry forward whatever is still accurate — the result should be one coherent, up-to-date file, not an append.

For your information, \`AGENTS.md\` is a file intended to be read by AI coding agents. Expect the reader of this file to know nothing about the project.

You should compose this file according to the actual project content. Do not make any assumptions or generalizations. Ensure the information is accurate and useful. You must use the natural language that is mainly used in the project's comments and documentation.

Popular sections that people usually write in \`AGENTS.md\` are:

- Project overview
- Build and test commands
- Code style guidelines
- Testing instructions
- Security considerations`;

/**
 * The directory holding `marketplace.json`, so the engine can resolve a
 * relative catalog `source` to a real plugin root. `KIMI_CODE_PLUGIN_MARKETPLACE_DIR`
 * pins it (tests, and installs that ship the catalog elsewhere); otherwise a
 * repo checkout is found by walking up from the cwd and from this module. A
 * packaged install has neither, so this answers `undefined` and the engine
 * falls back to its own cwd-relative lookup.
 */
export function resolvePluginMarketplaceDir(): string | undefined {
  const override = process.env['KIMI_CODE_PLUGIN_MARKETPLACE_DIR'];
  if (override !== undefined && override !== '') return override;
  const candidates = [
    join(process.cwd(), 'plugins'),
    resolve(import.meta.dirname, '../../../../plugins'),
  ];
  for (const dir of candidates) {
    if (existsSync(join(dir, 'marketplace.json'))) return dir;
  }
  return undefined;
}

/**
 * The host executable when it can host plugin node entries itself, i.e. only
 * under the packaged single-file binary (`bun build --compile`, marked by the
 * bundled `__KIMI_BUN_ASSETS__` asset map). Source (`bun src/main.ts`) and
 * `node dist/main.mjs` runs answer `undefined`: there the declared
 * `command: "node"` spawns as-is, and a Node runtime is available by
 * construction (node-dist) or in every dev checkout (bun).
 */
export function pluginNodeRunnerPath(): string | undefined {
  const view = globalThis as { Bun?: unknown; __KIMI_BUN_ASSETS__?: unknown };
  if (view.Bun === undefined || view.__KIMI_BUN_ASSETS__ === undefined) return undefined;
  return process.execPath;
}

/**
 * The manifest `agents` field of an installed plugin: one `./` directory or a
 * list of them (`docs/en/customization/plugins.md:282`). The engine's
 * `kimi.plugin.json` reader does not carry that key, so the manifest is read
 * here; an absent or unusable field falls back to the plugin root's `agents/`
 * directory, which is the documented auto-discovery.
 */
export function manifestAgentPaths(info: PluginInfo): readonly string[] {
  const manifestPath =
    typeof info.manifestPath === 'string' && info.manifestPath.length > 0
      ? info.manifestPath
      : typeof info.root === 'string'
        ? join(info.root, 'kimi.plugin.json')
        : undefined;
  if (manifestPath === undefined) return [];
  let manifest: unknown;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  } catch {
    return ['agents'];
  }
  const agents = (manifest as { agents?: unknown } | null)?.agents;
  const entries = typeof agents === 'string' ? [agents] : Array.isArray(agents) ? agents : [];
  const paths = entries.filter(
    (entry): entry is string => typeof entry === 'string' && entry.trim().length > 0,
  );
  return paths.length > 0 ? paths : ['agents'];
}

/**
 * Narrow an SDK trigger to the three the engine's swarm mode knows.
 *
 * The engine distinguishes `task` and `tool` (both close themselves at turn
 * end) from `manual` (which does not), so the distinction has to survive the
 * crossing. The SDK's type is wider than the engine's; `auto` and `system`
 * have no engine counterpart and mean the same thing to it as `manual` — the
 * mode stays open until the user leaves it.
 */
export function engineSwarmTrigger(trigger: SwarmModeTrigger): 'manual' | 'task' | 'tool' {
  return trigger === 'task' || trigger === 'tool' ? trigger : 'manual';
}

/** One subagent recovered from a tool result during a resume. */
export interface DiscoveredSubagent {
  readonly agentId: string;
  readonly summary: string;
}

/**
 * Recover the subagents a finished `Agent` / `AgentSwarm` call left behind, by
 * reading the tool result the transcript kept.
 *
 * Two result shapes, and they do not overlap:
 *
 * - `Agent` → `formatForegroundAgentSuccess`, a plain-text envelope
 *   (`agent_id: …` / `[summary]`). Exactly one subagent.
 * - `AgentSwarm` → `renderSwarmResults`, an `<agent_swarm_result>` XML block
 *   whose members are `<subagent agent_id="…" …>body</subagent>`. **N**
 *   subagents, and the ids are `agent_id="…"` with an equals sign — the
 *   single-agent `agent_id:` pattern never matches, which is why a resumed
 *   swarm used to come back with no members at all and got redrawn as plain
 *   subagent cards.
 *
 * The `outcome` attribute is read but not filtered on: a failed or aborted
 * member is still a member the user was shown, and dropping it would leave a
 * hole in the restored conversation. The body is the member's own result text,
 * which is the same text the live swarm card showed.
 */
export function discoverSubagentsFromToolResult(content: string): DiscoveredSubagent[] {
  if (content.includes('<agent_swarm_result>')) {
    return parseSwarmResultMembers(content);
  }
  const agentIdMatch = /(?:^|\n)agent_id:\s*([^\s]+)\s*(?=\n|$)/.exec(content);
  if (!agentIdMatch) return [];
  const summaryMatch = /(?:^|\n)\[summary\]\n([\s\S]*)$/.exec(content);
  return [{ agentId: agentIdMatch[1]!, summary: summaryMatch ? summaryMatch[1]! : '' }];
}

export function parseSwarmResultMembers(content: string): DiscoveredSubagent[] {
  const members: DiscoveredSubagent[] = [];
  const tag = /<subagent\b([^>]*)>/g;
  let match: RegExpExecArray | null;
  while ((match = tag.exec(content)) !== null) {
    const closeIndex = content.indexOf('</subagent>', tag.lastIndex);
    if (closeIndex < 0) break;
    const attrs = match[1] ?? '';
    const idMatch = /\bagent_id="([^"]*)"/.exec(attrs);
    if (idMatch && idMatch[1]) {
      members.push({
        agentId: idMatch[1],
        summary: content.slice(tag.lastIndex, closeIndex),
      });
    }
    tag.lastIndex = closeIndex + '</subagent>'.length;
  }
  return members;
}

export function resolveMcpServersForEngine(servers: Record<string, StoredMcpServerConfig>): Array<{
  name: string;
  transport: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  headers?: Record<string, string>;
  deferred?: boolean;
}> {
  const result: Array<{
    name: string;
    transport: string;
    command?: string;
    args?: string[];
    env?: Record<string, string>;
    url?: string;
    headers?: Record<string, string>;
    deferred?: boolean;
  }> = [];

  for (const [name, srv] of Object.entries(servers)) {
    if ((srv as { enabled?: boolean }).enabled === false) continue;
    // v2 `McpServerConfigSchema` infers the transport when the entry omits it
    // (`mcpCore/config-schema.ts`): a `command` means stdio, a `url` means
    // http. Requiring the field explicitly dropped every server written in the
    // standard MCP shape — `{"mcpServers":{"x":{"command":"…","args":[…]}}}` —
    // silently, so the session started with no MCP servers and nothing said so.
    const transport =
      srv.transport ??
      (typeof srv.command === 'string'
        ? 'stdio'
        : typeof srv.url === 'string'
          ? 'http'
          : undefined);
    if (transport === 'stdio' && srv.command) {
      result.push({
        name,
        transport: 'stdio',
        command: srv.command,
        ...(srv.args ? { args: srv.args } : {}),
        ...(srv.env ? { env: srv.env } : {}),
        ...(srv.deferred === true ? { deferred: true } : {}),
      });
    } else if ((transport === 'http' || transport === 'sse') && srv.url) {
      result.push({
        name,
        transport: 'sse',
        url: srv.url,
        ...(srv.headers ? { headers: srv.headers } : {}),
        ...(srv.deferred === true ? { deferred: true } : {}),
      });
    }
  }
  return result;
}
