import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';

import { afterEach, describe, expect, it, vi } from 'vitest';

import { createKimiHarness, ErrorCodes, type Event, type KimiError } from '#/index';

import { makeTempDir, removeTempDirs } from './session-runtime-helpers';
import { TEST_IDENTITY } from './test-identity';

// node-sdk/agent-core normalize paths to forward slashes (pathe). Mirror that
// in path assertions so they hold on Windows, where node:path produces
// backslashes.
const toPosix = (p: string): string => p.replaceAll('\\', '/');

const tempDirs: string[] = [];

afterEach(async () => {
  await removeTempDirs(tempDirs);
});

describe('Session plan, compact, usage, and resume APIs', () => {
  it('sets plan mode through manualEnterPlan and clears the active plan file', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-plan-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-plan-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const session = await harness.createSession({ id: 'ses_plan_runtime', workDir });

      const planOn = waitForSessionEvent(
        session,
        (event) => event.type === 'agent.status.updated' && event.planMode === true,
      );
      await session.setPlanMode(true);
      await expect(planOn).resolves.toMatchObject({
        type: 'agent.status.updated',
        planMode: true,
      });

      await expect(session.clearPlan()).resolves.toBeUndefined();
      await expect(session.getPlan()).resolves.toMatchObject({
        content: '',
      });
      await session.cancel();

      const planOff = waitForSessionEvent(
        session,
        (event) => event.type === 'agent.status.updated' && event.planMode === false,
      );
      await session.setPlanMode(false);
      await expect(planOff).resolves.toMatchObject({
        type: 'agent.status.updated',
        planMode: false,
      });
    } finally {
      await harness.close();
    }
  });

  it('validates setTowerMode boolean argument and records the coordinator mode', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-tower-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-tower-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const session = await harness.createSession({ id: 'ses_tower_runtime', workDir });

      await expect(session.setTowerMode('yes' as unknown as boolean)).rejects.toMatchObject({
        code: ErrorCodes.REQUEST_INVALID,
      });
      // The tower tools run engine-side natively; the flag records the
      // coordinator mode and must be reflected in the live status.
      await session.setTowerMode(true);
      await expect(session.getStatus()).resolves.toMatchObject({ towerMode: true });
      await session.setTowerMode(false);
      await expect(session.getStatus()).resolves.toMatchObject({ towerMode: false });
    } finally {
      await harness.close();
    }
  });

  it('prepares the plans directory without creating plan files on repeated toggles', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-plan-toggle-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-plan-toggle-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const session = await harness.createSession({ id: 'ses_plan_toggle_runtime', workDir });

      await session.setPlanMode(true);
      const firstPlan = await session.getPlan();
      if (firstPlan === null) throw new Error('expected first plan');
      const plansDir = dirname(firstPlan.path);
      await expect(markdownFiles(plansDir)).resolves.toEqual([]);

      await session.setPlanMode(false);
      await session.setPlanMode(true);
      const secondPlan = await session.getPlan();
      if (secondPlan === null) throw new Error('expected second plan');

      expect(secondPlan.path).not.toBe(firstPlan.path);
      expect(dirname(secondPlan.path)).toBe(plansDir);
      await expect(markdownFiles(plansDir)).resolves.toEqual([]);
    } finally {
      await harness.close();
    }
  });

  it('rejects manual compaction on an empty session with compaction.unable', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-compact-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-compact-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const session = await harness.createSession({ id: 'ses_compact_runtime', workDir });

      await expect(session.compact({ instruction: 'Keep important facts.' })).rejects.toMatchObject({
        code: 'compaction.unable',
      });
    } finally {
      await harness.close();
    }
  });

  it('returns current session usage totals', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-usage-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-usage-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const session = await harness.createSession({ id: 'ses_usage_runtime', workDir });

      await expect(session.getUsage()).resolves.toEqual({});
    } finally {
      await harness.close();
    }
  });

  it('groups a finished turn under its model and restores the buckets on resume', async () => {
    const { createServer } = await import('node:http');
    const calls = { count: 0 };
    const server = createServer((req, res) => {
      req.on('data', () => {});
      req.on('end', () => {
        if (req.url?.includes('/chat/completions') !== true) {
          res.writeHead(404).end();
          return;
        }
        calls.count += 1;
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
        res.write(chunk({ role: 'assistant', content: 'hi' }));
        res.write(chunk({}, 'stop'));
        // The usage-only chunk the engine's `stream_options.include_usage`
        // asks for; only the first request carries tokens so title generation
        // (if any later call happens) cannot double the counters.
        if (calls.count === 1) {
          res.write(
            `data: ${JSON.stringify({
              id: 'c',
              object: 'chat.completion.chunk',
              created: 0,
              model: 'mock',
              choices: [],
              usage: {
                prompt_tokens: 40,
                completion_tokens: 5,
                total_tokens: 45,
                prompt_tokens_details: { cached_tokens: 30 },
              },
            })}\n\n`,
          );
        }
        res.write('data: [DONE]\n\n');
        res.end();
      });
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    const address = server.address();
    const port = typeof address === 'object' && address !== null ? address.port : 0;

    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-usage-turn-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-usage-turn-work-');
    await writeFile(
      join(homeDir, 'config.toml'),
      `
default_model = "test-model"

[providers.local]
type = "openai"
base_url = "http://127.0.0.1:${port}/v1"
api_key = "sk-test"

[models.test-model]
provider = "local"
model = "test-model"
max_context_size = 200000
`,
      'utf-8',
    );
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const session = await harness.createSession({ id: 'ses_usage_turn', workDir });
      const ended = waitForSessionEvent(session, (event) => event.type === 'turn.ended');
      await session.prompt('hello');
      await expect(ended).resolves.toMatchObject({ reason: 'completed' });

      const expected = {
        byModel: {
          'test-model': { inputOther: 10, output: 5, inputCacheRead: 30, inputCacheCreation: 0 },
        },
        total: { inputOther: 10, output: 5, inputCacheRead: 30, inputCacheCreation: 0 },
        currentTurn: { inputOther: 10, output: 5, inputCacheRead: 30, inputCacheCreation: 0 },
      };
      // `turn.ended` reaches the event stream before the host's outcome fold
      // lands (the SDK settles the turn on the `turnOutcome` promise), so poll
      // instead of racing it.
      await vi.waitFor(async () => {
        await expect(session.getUsage()).resolves.toEqual(expected);
      });
      await session.close();

      // The durable slice: a resumed session reports the same buckets instead
      // of resetting to zero (v2's `UsageRecord` fold survives restarts).
      const resumed = await harness.resumeSession({ id: session.id });
      await expect(resumed.getUsage()).resolves.toEqual(expected);
    } finally {
      server.close();
      await harness.close();
    }
  });

  it('resumes a persisted session and restores runtime plan mode from wire history', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-resume-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-resume-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const created = await harness.createSession({
        id: 'ses_resume_runtime',
        workDir,
        model: 'test-model',
      });
      await created.setPlanMode(true);
      await expect(created.getPlan()).resolves.toMatchObject({
        content: '',
      });
      await created.close();
      expect(harness.getSession(created.id)).toBeUndefined();

      const resumed = await harness.resumeSession({ id: created.id });

      expect(resumed.id).toBe(created.id);
      expect(resumed.workDir).toBe(toPosix(workDir));
      await expect(resumed.getStatus()).resolves.toMatchObject({
        model: 'test-model',
        planMode: true,
      });
      await expect(resumed.getPlan()).resolves.toMatchObject({
        content: '',
        path: expect.stringContaining('/plans/'),
      });
      expect(harness.getSession(created.id)).toBe(resumed);
    } finally {
      await harness.close();
    }
  });

  it('forks a session, drops goal state, and returns an active fork session', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-fork-home-');
    const workDir = await makeTempDir(tempDirs, 'kimi-sdk-fork-work-');
    await writeTestConfig(homeDir);
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      const source = await harness.createSession({
        id: 'ses_fork_runtime_source',
        workDir,
        model: 'test-model',
        metadata: {
          source: true,
          goal: {
            goalId: 'source-goal',
            objective: 'source objective',
            status: 'active',
            turnsUsed: 0,
            tokensUsed: 0,
            budgetLimits: {},
          },
        },
      });
      await source.createGoal({ objective: 'source objective' });
      await source.setPlanMode(true);
      const sourcePlan = await source.getPlan();
      if (sourcePlan === null) throw new Error('expected source plan');
      await mkdir(dirname(sourcePlan.path), { recursive: true });
      await writeFile(sourcePlan.path, 'source plan', 'utf-8');

      const fork = await harness.forkSession({
        id: source.id,
        forkId: 'ses_fork_runtime_child',
        title: 'Forked runtime',
        metadata: {
          child: true,
          goal: {
            goalId: 'metadata-goal',
            objective: 'metadata objective',
            status: 'active',
            turnsUsed: 0,
            tokensUsed: 0,
            budgetLimits: {},
          },
        },
      });

      expect(fork.id).toBe('ses_fork_runtime_child');
      expect(fork.workDir).toBe(toPosix(workDir));
      await expect(fork.getStatus()).resolves.toMatchObject({ model: 'test-model' });
      expect(harness.getSession(fork.id)).toBe(fork);
      await expect(fork.getUsage()).resolves.toEqual({});

      const forkSummary = fork.summary;
      expect(forkSummary).toBeDefined();
      const forkPlan = await fork.getPlan();
      expect(forkPlan).toEqual({
        id: sourcePlan.id,
        content: 'source plan',
        path: toPosix(join(forkSummary!.sessionDir, 'agents', 'main', 'plans', `${sourcePlan.id}.md`)),
      });
      expect(forkPlan?.path).not.toBe(sourcePlan.path);
      await expect(fork.getGoal()).resolves.toEqual({ goal: null });
      const forkState = JSON.parse(
        await readFile(join(forkSummary!.sessionDir, 'session-meta.json'), 'utf-8'),
      ) as {
        title?: string;
        forkedFrom?: string;
        custom?: Record<string, unknown>;
      };
      expect(forkState.title).toBe('Forked runtime');
      expect(forkState.forkedFrom).toBe(source.id);
      expect(forkState.custom).toMatchObject({ source: true, child: true });
      expect(forkState.custom).not.toHaveProperty('goal');
    } finally {
      await harness.close();
    }
  });

  it('rejects an empty resume id', async () => {
    const homeDir = await makeTempDir(tempDirs, 'kimi-sdk-resume-empty-home-');
    const harness = createKimiHarness({ homeDir, identity: TEST_IDENTITY });

    try {
      await expect(harness.resumeSession({ id: '   ' })).rejects.toMatchObject({
        name: 'KimiError',
        code: 'session.id_empty',
      } satisfies Partial<KimiError>);
    } finally {
      await harness.close();
    }
  });
});

function waitForSessionEvent(
  session: { onEvent(listener: (event: Event) => void): () => void },
  predicate: (event: Event) => boolean,
): Promise<Event> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      unsubscribe();
      reject(new Error('Timed out waiting for session event'));
    }, 1_000);
    const unsubscribe = session.onEvent((event) => {
      if (!predicate(event)) return;
      clearTimeout(timeout);
      unsubscribe();
      resolve(event);
    });
  });
}

async function writeTestConfig(homeDir: string): Promise<void> {
  await writeFile(
    join(homeDir, 'config.toml'),
    `
default_model = "test-model"

[providers.local]
type = "openai"
base_url = "https://example.test/v1"
api_key = "sk-test"

[models.test-model]
provider = "local"
model = "test-model"
max_context_size = 200000
`,
    'utf-8',
  );
}

async function markdownFiles(dir: string): Promise<string[]> {
  const entries = await readdir(dir);
  return entries.filter((entry) => entry.endsWith('.md')).toSorted();
}
