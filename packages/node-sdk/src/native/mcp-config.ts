/**
 * Layered MCP server config (`mcp.json`) loading, ported from v2
 * `app/mcpConfig/configLoader.ts`.
 *
 * Three layers merge in order, later winning:
 *
 *   user         `<KIMI_CODE_HOME>/mcp.json`
 *   projectRoot  `<git-worktree-root>/.mcp.json`
 *   project      `<workDir>/.kimi-code/mcp.json`
 *
 * The project layers are gated on workspace trust by the caller. A stdio
 * entry read from `.mcp.json` defaults its `cwd` to the worktree root (and a
 * relative declared `cwd` resolves against it), so a script shipped in the
 * repository can find its sibling files no matter where the process started.
 *
 * Reads are tolerant: a missing, empty, malformed, or wrongly-shaped layer
 * contributes no servers. v2's loader throws `config.invalid` from the
 * workspace service, but every host entry point in this fork treats MCP config
 * as best-effort (the print path wraps it in try/catch, and the user-global
 * reader for the engine pipeline is already tolerant), so one broken file in
 * a checkout must not brick the session.
 */
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

export interface StoredMcpServerConfig {
  transport?: 'stdio' | 'http' | 'sse';
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  headers?: Record<string, string>;
  /** Working directory for a stdio server (v2 `cwd`). */
  cwd?: string;
  auth?: 'oauth';
  bearerTokenEnvVar?: string;
  enabled?: boolean;
  name?: string;
  /** Keep this server's tools out of the top-level list; load them via `select_tools`. */
  deferred?: boolean;
  enabledTools?: string[];
  disabledTools?: string[];
  startupTimeoutMs?: number;
  toolTimeoutMs?: number;
}

/** One entry in the engine's napi `mcpServers` wire shape (camelCase). */
export interface EngineMcpServerConfig {
  name: string;
  transport: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  headers?: Record<string, string>;
  cwd?: string;
  bearerTokenEnvVar?: string;
  enabledTools?: string[];
  disabledTools?: string[];
  startupTimeoutMs?: number;
  toolTimeoutMs?: number;
  deferred?: boolean;
}

/**
 * The checkout root: the nearest ancestor (or the start itself) holding a
 * `.git` entry — a directory in a clone, a file in a worktree. Falls back to
 * the start when no ancestor is a checkout.
 */
export function findWorkTreeRoot(start: string): string {
  const initial = resolve(start);
  let current = initial;
  for (;;) {
    if (existsSync(join(current, '.git'))) return current;
    const parent = dirname(current);
    if (parent === current) return initial;
    current = parent;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Read one layer; `stdioCwdBase` applies v2's `.mcp.json` cwd normalization. */
function readMcpLayer(
  path: string,
  stdioCwdBase?: string,
): Record<string, StoredMcpServerConfig> {
  let text: string;
  try {
    text = readFileSync(path, 'utf8');
  } catch {
    return Object.create(null);
  }
  if (text.trim().length === 0) return Object.create(null);
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return Object.create(null);
  }
  if (!isRecord(raw)) return Object.create(null);
  const servers = raw['mcpServers'];
  if (!isRecord(servers)) return Object.create(null);

  const out: Record<string, StoredMcpServerConfig> = Object.create(null);
  for (const [name, value] of Object.entries(servers)) {
    if (!isRecord(value)) continue;
    const config = value as StoredMcpServerConfig;
    if (
      stdioCwdBase !== undefined &&
      (config.transport ?? (typeof config.command === 'string' ? 'stdio' : undefined)) === 'stdio'
    ) {
      out[name] = {
        ...config,
        cwd: config.cwd === undefined ? stdioCwdBase : resolve(stdioCwdBase, config.cwd),
      };
    } else {
      out[name] = config;
    }
  }
  return out;
}

/** Merge the user and project layers; project layers need `includeProject`. */
export function loadMcpServerLayers(input: {
  readonly homeDir: string;
  readonly workDir: string;
  readonly includeProject: boolean;
}): Record<string, StoredMcpServerConfig> {
  const user = readMcpLayer(join(input.homeDir, 'mcp.json'));
  if (!input.includeProject) return user;

  const root = findWorkTreeRoot(input.workDir);
  const projectRoot = readMcpLayer(join(root, '.mcp.json'), root);
  const project = readMcpLayer(join(resolve(input.workDir), '.kimi-code', 'mcp.json'));
  return Object.assign(Object.create(null), user, projectRoot, project);
}

/**
 * Flatten the merged entries into the engine's wire shape. A missing
 * `transport` is inferred (v2 `McpServerConfigSchema`): `command` means stdio,
 * `url` means streamable HTTP. A legacy `sse` entry keeps `sse`; a bare `url`
 * is `http`, not `sse`.
 */
export function resolveMcpServersForEngine(
  servers: Record<string, StoredMcpServerConfig>,
): EngineMcpServerConfig[] {
  const result: EngineMcpServerConfig[] = [];

  for (const [name, srv] of Object.entries(servers)) {
    if (srv.enabled === false) continue;
    const transport =
      srv.transport ??
      (typeof srv.command === 'string' ? 'stdio' : typeof srv.url === 'string' ? 'http' : undefined);
    const common = {
      ...(srv.enabledTools ? { enabledTools: srv.enabledTools } : {}),
      ...(srv.disabledTools ? { disabledTools: srv.disabledTools } : {}),
      ...(srv.startupTimeoutMs ? { startupTimeoutMs: srv.startupTimeoutMs } : {}),
      ...(srv.toolTimeoutMs ? { toolTimeoutMs: srv.toolTimeoutMs } : {}),
      ...(srv.deferred === true ? { deferred: true } : {}),
    };
    if (transport === 'stdio' && srv.command) {
      result.push({
        name,
        transport: 'stdio',
        command: srv.command,
        ...(srv.args ? { args: srv.args } : {}),
        ...(srv.env ? { env: srv.env } : {}),
        ...(srv.cwd ? { cwd: srv.cwd } : {}),
        ...common,
      });
    } else if ((transport === 'http' || transport === 'sse') && srv.url) {
      result.push({
        name,
        transport,
        url: srv.url,
        ...(srv.headers ? { headers: srv.headers } : {}),
        ...(srv.bearerTokenEnvVar ? { bearerTokenEnvVar: srv.bearerTokenEnvVar } : {}),
        ...common,
      });
    }
  }
  return result;
}
