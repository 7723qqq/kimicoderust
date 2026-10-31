import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { findKimiAgentAddon } from '@moonshot-ai/kimi-agent/session-handle';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createKimiHarnessNative, type KimiHarness } from '../src/index';
import { TEST_IDENTITY } from './test-identity';

const hasNativeAddon = findKimiAgentAddon() !== null;

type HarnessSession = Awaited<ReturnType<KimiHarness['createSession']>>;

function waitForTurnEnded(session: HarnessSession): Promise<{ readonly reason?: string }> {
  return new Promise((resolve) => {
    const unsubscribe = session.onEvent((event) => {
      if (event.type === 'turn.ended') {
        unsubscribe();
        resolve(event);
      }
    });
  });
}

function textOf(message: {
  readonly content: string | readonly { readonly type: string }[];
}): string {
  if (typeof message.content === 'string') return message.content;
  return message.content
    .map((part) => (part.type === 'text' ? String((part as { text?: string }).text ?? '') : ''))
    .join(' ');
}

function originKind(message: { readonly origin?: unknown }): string | undefined {
  return (message.origin as { readonly kind?: string } | undefined)?.kind;
}

describe.skipIf(!hasNativeAddon)('session undo semantics', () => {
  let homeDir: string;
  let workDir: string;
  let harness: KimiHarness;
  let server: import('node:http').Server;

  beforeEach(async () => {
    const { createServer } = await import('node:http');
    server = createServer((req, res) => {
      req.on('data', () => {});
      req.on('end', () => {
        if (req.url?.includes('/chat/completions') !== true) {
          res.writeHead(404).end();
          return;
        }
        res.writeHead(200, {
          'content-type': 'text/event-stream',
          'cache-control': 'no-cache',
          connection: 'close',
        });
        const chunk = (delta: Record<string, unknown>, finish: string | null = null): string =>
          `data: ${JSON.stringify({
            id: 'c',
            object: 'chat.completion.chunk',
            created: 0,
            model: 'mock',
            choices: [{ index: 0, delta, finish_reason: finish }],
          })}\n\n`;
        res.write(chunk({ role: 'assistant', content: 'ok' }));
        res.write(chunk({}, 'stop'));
        res.write('data: [DONE]\n\n');
        res.end();
      });
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    const address = server.address();
    const port = typeof address === 'object' && address !== null ? address.port : 0;
    homeDir = mkdtempSync(join(tmpdir(), 'kimi-sdk-undo-home-'));
    workDir = mkdtempSync(join(tmpdir(), 'kimi-sdk-undo-work-'));
    writeFileSync(
      join(homeDir, 'config.toml'),
      `default_model = "test-model"

[providers.local]
type = "openai"
base_url = "http://127.0.0.1:${port}/v1"
api_key = "sk-test"

[models.test-model]
provider = "local"
model = "test-model"
max_context_size = 200000
`,
    );
    harness = createKimiHarnessNative({ homeDir, identity: TEST_IDENTITY });
  });

  afterEach(async () => {
    await harness.close();
    server.close();
    for (const dir of [homeDir, workDir]) {
      for (let attempt = 0; ; attempt += 1) {
        try {
          rmSync(dir, { recursive: true, force: true });
          break;
        } catch (error) {
          if (attempt >= 10) throw error;
          await new Promise((resolve) => setTimeout(resolve, 200));
        }
      }
    }
  });

  it('undoes user anchors, skips injections, and refuses an unavailable count', async () => {
    const session = await harness.createSession({ id: 'ses_undo', workDir });

    let ended = waitForTurnEnded(session);
    await session.prompt('one');
    await expect(ended).resolves.toMatchObject({ reason: 'completed' });

    ended = waitForTurnEnded(session);
    await session.prompt('two');
    await expect(ended).resolves.toMatchObject({ reason: 'completed' });

    const before = await session.getContext();
    const userTurns = before.history.filter(
      (message) => message.role === 'user' && originKind(message) !== 'injection',
    );
    expect(userTurns).toHaveLength(2);
    expect(before.history.some((message) => originKind(message) === 'injection')).toBe(true);

    await session.undoHistory(1);
    const after = await session.getContext();
    const retained = after.history
      .filter((message) => message.role === 'user' && originKind(message) !== 'injection')
      .map(textOf);
    expect(retained.some((text) => text.includes('one'))).toBe(true);
    expect(retained.some((text) => text.includes('two'))).toBe(false);

    // Only one anchor remains; asking for two must fail rather than silently
    // undoing one.
    await expect(session.undoHistory(2)).rejects.toMatchObject({
      code: 'session.undo_unavailable',
    });

    await session.undoHistory(1);
    await expect(session.undoHistory(1)).rejects.toMatchObject({
      code: 'session.undo_unavailable',
    });
    await session.close();
  }, 30_000);
});
