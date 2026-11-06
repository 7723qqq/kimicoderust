import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { SDKRpcClientNative } from '../src/index';
import { TEST_IDENTITY } from './test-identity';

const ENV = 'KIMI_CODE_TRUST_WORKSPACE';

describe('SDKRpcClientNative workspace trust', () => {
  const dirs: string[] = [];
  const clients: SDKRpcClientNative[] = [];

  afterEach(async () => {
    delete process.env[ENV];
    // Close first: the plugin registry holds `<homeDir>/sessions.db` open, and
    // Windows refuses to remove a directory with a locked file in it.
    while (clients.length > 0) await clients.pop()!.close();
    while (dirs.length > 0) rmSync(dirs.pop()!, { recursive: true, force: true });
  });

  function client(
    trusted: string[] = [],
    configToml?: string,
  ): { rpc: SDKRpcClientNative; workDir: string; homeDir: string } {
    const homeDir = mkdtempSync(join(tmpdir(), 'kimi-trust-'));
    dirs.push(homeDir);
    writeFileSync(join(homeDir, 'trusted-workspaces.json'), JSON.stringify(trusted));
    if (configToml !== undefined) writeFileSync(join(homeDir, 'config.toml'), configToml);
    const workDir = mkdtempSync(join(tmpdir(), 'kimi-trust-ws-'));
    dirs.push(workDir);
    const rpc = new SDKRpcClientNative({ homeDir, identity: TEST_IDENTITY });
    clients.push(rpc);
    return { rpc, workDir, homeDir };
  }

  it('fails closed for a workspace with no record and no env', async () => {
    const { rpc, workDir } = client();
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(false);
  });

  it('honors [workspace] require_trust and reloads it on every query', async () => {
    // A falsy process override is independent: it must not re-enable the gate
    // while the config gate is disabled (upstream #4517).
    process.env[ENV] = '0';
    const { rpc, workDir, homeDir } = client([], '[workspace]\nrequire_trust = false\n');
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(true);

    writeFileSync(join(homeDir, 'config.toml'), '[workspace]\nrequire_trust = true\n');
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(false);

    writeFileSync(join(homeDir, 'config.toml'), '[workspace]\nrequire_trust = false\n');
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(true);
  });

  it('trusts a workspace the user has recorded', async () => {
    const { rpc, workDir } = client();
    await rpc.trustWorkspace(workDir);
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(true);
  });

  it('lets KIMI_CODE_TRUST_WORKSPACE outrank the recorded decision', async () => {
    // Upstream #4059: the env ORs over the persisted record, so untrusting has
    // no observable effect while it is set. That is the documented trade, and
    // the only way to opt out is to unset the variable.
    const { rpc, workDir } = client();
    process.env[ENV] = '1';
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(true);

    await rpc.trustWorkspace(workDir);
    expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted).toBe(true);
  });

  it('accepts every truthy spelling and ignores the rest', async () => {
    for (const value of ['1', 'true', 'yes', 'on', 'ON', ' true ']) {
      const { rpc, workDir } = client();
      process.env[ENV] = value;
      expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted, value).toBe(true);
      await rpc.close();
      clients.pop();
      rmSync(dirs.pop()!, { recursive: true, force: true });
      rmSync(dirs.pop()!, { recursive: true, force: true });
    }
  });

  it('does not trust on a falsy or unrecognised spelling', async () => {
    // A typo must leave the normal prompt in place rather than silently
    // trusting every workspace the agent ever opens.
    for (const value of ['0', 'false', 'no', 'off', 'maybe', '']) {
      const { rpc, workDir } = client();
      process.env[ENV] = value;
      expect((await rpc.getWorkspaceTrustInfo(workDir)).trusted, value).toBe(false);
      await rpc.close();
      clients.pop();
      rmSync(dirs.pop()!, { recursive: true, force: true });
      rmSync(dirs.pop()!, { recursive: true, force: true });
    }
  });

  /**
   * The disclosure is what a user judges a directory on before trusting it, so
   * it has to describe the *project's* servers — and only their safe surface.
   */
  it('discloses the project MCP servers trusting would enable', async () => {
    const { rpc, workDir } = client();
    writeFileSync(
      join(workDir, '.mcp.json'),
      JSON.stringify({
        mcpServers: {
          local: {
            command: 'node',
            args: ['server.js'],
            cwd: '/tmp/x',
            // Secrets live here. The prompt renders before the workspace is
            // trusted, so they must not reach it.
            env: { API_KEY: 'sk-do-not-disclose' },
          },
          remote: { transport: 'sse', url: 'https://example.test/sse' },
          undescribable: { note: 'neither command nor url' },
        },
      }),
    );

    const info = await rpc.getWorkspaceTrustInfo(workDir);
    expect(info.trusted).toBe(false);
    expect(info.gatedMcpServers.map((s) => s.name).toSorted()).toEqual(['local', 'remote']);

    const local = info.gatedMcpServers.find((s) => s.name === 'local');
    expect(local).toMatchObject({
      transport: 'stdio',
      command: 'node',
      args: ['server.js'],
      cwd: '/tmp/x',
    });
    // No env key at all: not merely empty, absent from the shape.
    expect(Object.keys(local ?? {})).not.toContain('env');
    expect(JSON.stringify(info.gatedMcpServers)).not.toContain('sk-do-not-disclose');

    expect(info.gatedMcpServers.find((s) => s.name === 'remote')).toMatchObject({
      transport: 'sse',
      url: 'https://example.test/sse',
    });
    // A transport it cannot infer is not guessed.
    expect(info.gatedMcpServers.some((s) => s.name === 'undescribable')).toBe(false);
  });

  it('reads the team-shared .kimi-code/mcp.json as well as the root one', async () => {
    const { rpc, workDir } = client();
    writeFileSync(
      join(workDir, '.mcp.json'),
      JSON.stringify({ mcpServers: { fromRoot: { command: 'a' } } }),
    );
    const nested = join(workDir, '.kimi-code');
    mkdirSync(nested, { recursive: true });
    writeFileSync(
      join(nested, 'mcp.json'),
      JSON.stringify({ mcpServers: { fromNested: { command: 'b' } } }),
    );

    const info = await rpc.getWorkspaceTrustInfo(workDir);
    expect(info.gatedMcpServers.map((s) => s.name).toSorted()).toEqual(['fromNested', 'fromRoot']);
  });

  /**
   * The rest of the disclosure (upstream #4056 `trustDisclosureService`): the
   * instruction files a trusted session would load and the extra directories
   * it would reach. These come from the engine, not from a host-side re-parse,
   * so the drift the two copies used to have cannot recur.
   */
  it('discloses the instruction files and extra directories trusting would activate', async () => {
    const { rpc, workDir } = client();
    writeFileSync(join(workDir, 'AGENTS.md'), '# project instructions\n');

    const outside = join(workDir, '..', `outside-${process.pid}`);
    mkdirSync(outside, { recursive: true });
    mkdirSync(join(workDir, '.kimi-code'), { recursive: true });
    writeFileSync(
      join(workDir, '.kimi-code', 'local.toml'),
      `[workspace]\nadditional_dir = [${JSON.stringify(outside.replace(/\\/g, '/'))}]\n`,
    );

    const info = await rpc.getWorkspaceTrustInfo(workDir);
    expect(info.trusted).toBe(false);
    expect(info.instructionSources?.agentsMdPaths.some((p) => p.endsWith('AGENTS.md'))).toBe(true);
    expect(info.gatedAdditionalDirs?.length).toBe(1);
    expect(info.additionalDirSources?.some((p) => p.endsWith('local.toml'))).toBe(true);
    // Every listed path is also in the deduplicated union.
    for (const p of info.instructionSources?.agentsMdPaths ?? []) {
      expect(info.instructionSources?.paths).toContain(p);
    }
  });

  it('discloses a directory the user is about to add', async () => {
    const { rpc, workDir } = client();
    const pending = join(workDir, '..', `pending-${process.pid}`);
    mkdirSync(pending, { recursive: true });

    const info = await rpc.getWorkspaceTrustInfo(workDir, { additionalDirs: [pending] });
    expect(info.gatedAdditionalDirs?.length).toBe(1);
    // It is not in any config file yet, so it names itself as its source.
    expect(info.additionalDirSources?.length).toBe(1);
  });

  it('discloses nothing once the workspace is trusted', async () => {
    const { rpc, workDir } = client();
    writeFileSync(join(workDir, 'AGENTS.md'), '# project instructions\n');
    await rpc.trustWorkspace(workDir);

    const info = await rpc.getWorkspaceTrustInfo(workDir);
    expect(info.trusted).toBe(true);
    expect(info.gatedMcpServers).toEqual([]);
    expect(info.gatedAdditionalDirs).toBeUndefined();
  });

  it('discloses nothing for a workspace with no MCP config, and does not throw on a broken one', async () => {
    const { rpc, workDir } = client();
    expect((await rpc.getWorkspaceTrustInfo(workDir)).gatedMcpServers).toEqual([]);

    writeFileSync(join(workDir, '.mcp.json'), '{ not json');
    expect((await rpc.getWorkspaceTrustInfo(workDir)).gatedMcpServers).toEqual([]);
  });
});
