/**
 * Layered `mcp.json` loading (v2 `app/mcpConfig/configLoader.ts`): user,
 * project-root, and project layers merge in that order, the project layers
 * carry v2's stdio `cwd` normalization, and the engine wire shape keeps a
 * bare URL as streamable HTTP instead of collapsing it to legacy SSE.
 */
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import {
  findWorkTreeRoot,
  loadMcpServerLayers,
  resolveMcpServersForEngine,
} from '../src/native/mcp-config';

const dirs: string[] = [];

afterEach(() => {
  while (dirs.length > 0) rmSync(dirs.pop()!, { recursive: true, force: true });
});

function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

function writeJson(path: string, value: unknown): void {
  mkdirSync(join(path, '..'), { recursive: true });
  writeFileSync(path, JSON.stringify(value), 'utf-8');
}

function writeRaw(path: string, text: string): void {
  mkdirSync(join(path, '..'), { recursive: true });
  writeFileSync(path, text, 'utf-8');
}

describe('layered MCP config', () => {
  it('loads only the user layer when project config is not trusted', () => {
    const homeDir = tempDir('kimi-mcp-home-');
    const workDir = tempDir('kimi-mcp-work-');
    writeJson(join(homeDir, 'mcp.json'), {
      mcpServers: { user: { command: 'user-cmd' } },
    });
    writeJson(join(workDir, '.kimi-code', 'mcp.json'), {
      mcpServers: { project: { command: 'project-cmd' } },
    });

    const servers = loadMcpServerLayers({ homeDir, workDir, includeProject: false });
    expect(Object.keys(servers)).toEqual(['user']);
  });

  it('merges user < .mcp.json < .kimi-code/mcp.json', () => {
    const homeDir = tempDir('kimi-mcp-home-');
    const root = tempDir('kimi-mcp-root-');
    mkdirSync(join(root, '.git'));
    const workDir = join(root, 'packages', 'app');
    mkdirSync(workDir, { recursive: true });

    writeJson(join(homeDir, 'mcp.json'), {
      mcpServers: {
        shared: { command: 'user-cmd' },
        'user-only': { command: 'user-only-cmd' },
      },
    });
    writeJson(join(root, '.mcp.json'), {
      mcpServers: {
        shared: { command: 'root-cmd' },
        'root-only': { command: 'root-only-cmd' },
      },
    });
    writeJson(join(workDir, '.kimi-code', 'mcp.json'), {
      mcpServers: {
        shared: { command: 'project-cmd' },
        'project-only': { command: 'project-only-cmd' },
      },
    });

    const servers = loadMcpServerLayers({ homeDir, workDir, includeProject: true });
    expect((servers['shared'] as { command: string }).command).toBe('project-cmd');
    expect((servers['user-only'] as { command: string }).command).toBe('user-only-cmd');
    expect((servers['root-only'] as { command: string }).command).toBe('root-only-cmd');
    expect((servers['project-only'] as { command: string }).command).toBe('project-only-cmd');
  });

  it('defaults and resolves stdio cwd for the .mcp.json layer only', () => {
    const homeDir = tempDir('kimi-mcp-home-');
    const root = tempDir('kimi-mcp-root-');
    mkdirSync(join(root, '.git'));
    writeJson(join(homeDir, 'mcp.json'), {
      mcpServers: { user: { command: 'user-cmd' } },
    });
    writeJson(join(root, '.mcp.json'), {
      mcpServers: {
        defaulted: { command: 'root-cmd' },
        relative: { command: 'root-cmd', cwd: 'tools' },
        absolute: { command: 'root-cmd', cwd: resolve(root, 'abs') },
        remote: { url: 'https://example.test/mcp' },
      },
    });
    writeJson(join(root, '.kimi-code', 'mcp.json'), {
      mcpServers: { project: { command: 'project-cmd', cwd: 'relative-dir' } },
    });

    const servers = loadMcpServerLayers({ homeDir, workDir: root, includeProject: true });
    expect(servers['user']).toMatchObject({ command: 'user-cmd' });
    expect(servers['user']?.cwd).toBeUndefined();
    expect(servers['defaulted']?.cwd).toBe(resolve(root));
    expect(servers['relative']?.cwd).toBe(resolve(root, 'tools'));
    expect(servers['absolute']?.cwd).toBe(resolve(root, 'abs'));
    expect(servers['remote']?.cwd).toBeUndefined();
    // The project layer resolves its declared cwd against the process cwd,
    // exactly as v2 leaves it (only `.mcp.json` carries a cwd base).
    expect(servers['project']?.cwd).toBe('relative-dir');
  });

  it('finds the worktree root and falls back to the start', () => {
    const root = tempDir('kimi-mcp-root-');
    mkdirSync(join(root, '.git'));
    const nested = join(root, 'a', 'b');
    mkdirSync(nested, { recursive: true });
    expect(findWorkTreeRoot(nested)).toBe(resolve(root));

    const plain = tempDir('kimi-mcp-plain-');
    expect(findWorkTreeRoot(plain)).toBe(resolve(plain));
  });

  it('treats a malformed layer as empty instead of failing the session', () => {
    const homeDir = tempDir('kimi-mcp-home-');
    const root = tempDir('kimi-mcp-root-');
    mkdirSync(join(root, '.git'));
    writeJson(join(homeDir, 'mcp.json'), {
      mcpServers: { user: { command: 'user-cmd' } },
    });
    writeRaw(join(root, '.mcp.json'), '{ not json');

    const servers = loadMcpServerLayers({ homeDir, workDir: root, includeProject: true });
    expect(Object.keys(servers)).toEqual(['user']);
  });
});

describe('engine MCP wire shape', () => {
  it('keeps a bare url as streamable HTTP and a declared sse as SSE', () => {
    const servers = resolveMcpServersForEngine({
      bare: { url: 'https://example.test/mcp' },
      legacy: { transport: 'sse', url: 'https://example.test/sse' },
      explicit: { transport: 'http', url: 'https://example.test/http' },
    });
    expect(servers).toMatchObject([
      { name: 'bare', transport: 'http' },
      { name: 'legacy', transport: 'sse' },
      { name: 'explicit', transport: 'http' },
    ]);
  });

  it('skips disabled servers and forwards per-server controls', () => {
    const servers = resolveMcpServersForEngine({
      off: { enabled: false, command: 'off-cmd' },
      stdio: {
        command: 'srv',
        args: ['--x'],
        env: { A: '1' },
        cwd: '/tmp/srv',
        deferred: true,
        enabledTools: ['a'],
        disabledTools: ['b'],
        startupTimeoutMs: 1000,
        toolTimeoutMs: 2000,
      },
      remote: {
        transport: 'http',
        url: 'https://example.test/mcp',
        bearerTokenEnvVar: 'TOKEN',
        deferred: true,
      },
    });

    expect(servers.map((server) => server.name)).toEqual(['stdio', 'remote']);
    expect(servers[0]).toEqual({
      name: 'stdio',
      transport: 'stdio',
      command: 'srv',
      args: ['--x'],
      env: { A: '1' },
      cwd: '/tmp/srv',
      enabledTools: ['a'],
      disabledTools: ['b'],
      startupTimeoutMs: 1000,
      toolTimeoutMs: 2000,
      deferred: true,
    });
    expect(servers[1]).toMatchObject({
      transport: 'http',
      bearerTokenEnvVar: 'TOKEN',
      deferred: true,
    });
  });

  it('skips entries that cannot name a transport', () => {
    expect(resolveMcpServersForEngine({ empty: {}, bad: { args: ['x'] } })).toEqual([]);
  });

  it('forwards an explicit deferred=false so it beats [mcp] defaultDeferred', () => {
    const servers = resolveMcpServersForEngine({
      inherit: { command: 'a' },
      inline: { command: 'b', deferred: false },
      deferred: { command: 'c', deferred: true },
    });
    expect(servers[0]).not.toHaveProperty('deferred');
    expect(servers[1]).toMatchObject({ name: 'inline', deferred: false });
    expect(servers[2]).toMatchObject({ name: 'deferred', deferred: true });
  });
});
