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

/**
 * A tool call the mock model makes on its next request, consumed once. An
 * active goal keeps the engine's pump continuing turns, so a test that wants a
 * settled session has to let the model finish the goal in the same turn.
 */
let queuedToolCall: { readonly name: string; readonly arguments: string } | null = null;

describe.skipIf(!hasNativeAddon)('session undo semantics', () => {
  let homeDir: string;
  let workDir: string;
  let harness: KimiHarness;
  let server: import('node:http').Server;

  beforeEach(async () => {
    queuedToolCall = null;
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
        const toolCall = queuedToolCall;
        queuedToolCall = null;
        if (toolCall !== null) {
          res.write(
            chunk({
              role: 'assistant',
              content: '',
              tool_calls: [
                { index: 0, id: 'call_1', type: 'function', function: toolCall },
              ],
            }),
          );
          res.write(chunk({}, 'tool_calls'));
          res.write('data: [DONE]\n\n');
          res.end();
          return;
        }
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

    // v2 `availability`: two anchors, and the engine holds the two
    // checkpoints those turns pushed.
    await expect(session.getUndoAvailability()).resolves.toEqual({
      maxTurns: 2,
      stoppedAtCompaction: false,
    });

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

  /**
   * The engine's rollback carries the goal with it, and the host's cached copy
   * is what the next turn's goal provider hands back to the engine — a stale
   * one let `sync_goal_from_context` write the undone fold straight back, so
   * the goal silently disagreed with the transcript it had just been rewound
   * with. `meta.goal` now follows the store the rollback restored.
   */
  it('restores the goal the cut turn started with', async () => {
    const session = await harness.createSession({ id: 'ses_undo_goal', workDir });
    await session.createGoal({ objective: 'Ship the undo tail' });
    expect((await session.getGoal()).goal?.status).toBe('active');

    // The model completes the goal in its turn, so the pump does not keep
    // continuing an active goal and the session settles for the undo.
    queuedToolCall = {
      name: 'UpdateGoal',
      arguments: JSON.stringify({ status: 'complete' }),
    };
    const ended = waitForTurnEnded(session);
    await session.prompt('one');
    await expect(ended).resolves.toMatchObject({ reason: 'completed' });
    // The store moved during the turn and the host adopted the engine's
    // `goal.updated` — the folded goal is the state undo has to rewind.
    expect((await session.getGoal()).goal?.status).toBe('complete');

    const updates: (string | null)[] = [];
    const unsubscribe = session.onEvent((event) => {
      if (event.type === 'goal.updated') updates.push(event.snapshot?.status ?? null);
    });
    await session.undoHistory(1);
    unsubscribe();

    // The rollback restored the goal as the cut turn started it, and the host
    // followed instead of handing the completed copy back to the engine.
    expect((await session.getGoal()).goal?.status).toBe('active');
    // The panel has to see the rewind as an update, or it keeps showing the
    // goal the engine no longer holds.
    expect(updates).toEqual(['active']);
    await session.close();
  }, 30_000);

  /**
   * v2 refuses an undo the state checkpoints cannot cover (`checkpoint_lost`)
   * instead of cutting the transcript and rolling back whatever is left: the
   * two halves of an undo have to move together or the panels describe a state
   * the history no longer has. A fork is the case that reaches this — it copies
   * the transcript and starts with an empty checkpoint stack.
   */
  it('refuses an undo the state checkpoints cannot cover', async () => {
    const session = await harness.createSession({ id: 'ses_undo_fork_src', workDir });
    const ended = waitForTurnEnded(session);
    await session.prompt('one');
    await expect(ended).resolves.toMatchObject({ reason: 'completed' });
    // The source session has the one checkpoint its turn pushed.
    await expect(session.undoHistory(2)).rejects.toMatchObject({
      code: 'session.undo_unavailable',
      details: { reason: 'insufficient' },
    });

    // `prompt` submits and returns, and the session's busy flag clears only
    // when the turn's outcome settles — a beat after `turn.ended`. Wait for
    // the fork to be admitted instead of racing that flag.
    let forked: Awaited<ReturnType<KimiHarness['forkSession']>> | undefined;
    let forkError: unknown;
    for (let attempt = 0; attempt < 100 && forked === undefined; attempt += 1) {
      try {
        forked = await harness.forkSession({
          id: 'ses_undo_fork_src',
          forkId: 'ses_undo_fork',
        });
      } catch (error) {
        forkError = error;
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
    }
    if (forked === undefined) throw forkError;
    const before = await forked.getContext();
    expect(
      before.history.filter((message) => message.role === 'user' && originKind(message) !== 'injection'),
    ).toHaveLength(1);

    // The menu reads the same number the command enforces, so it never offers
    // a turn this session cannot roll back.
    await expect(forked.getUndoAvailability()).resolves.toEqual({
      maxTurns: 0,
      stoppedAtCompaction: false,
    });
    await expect(forked.undoHistory(1)).rejects.toMatchObject({
      code: 'session.undo_unavailable',
      details: { reason: 'checkpoint_lost', requestedCount: 1, undoableCount: 0 },
    });
    // Nothing moved: the refusing undo leaves the transcript where it was.
    const after = await forked.getContext();
    expect(after.history).toHaveLength(before.history.length);
    await session.close();
    await forked.close();
  }, 30_000);
});
