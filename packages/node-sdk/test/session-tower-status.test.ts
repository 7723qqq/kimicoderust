import { describe, expect, it } from 'vitest';

import { ErrorCodes, SDKRpcClientBase, Session } from '../src/index';
import { renderTowerStatusSummary } from '../src/native/tower-status';

describe('Session.getTowerStatus', () => {
  it('defaults to not_implemented on backends without support', async () => {
    const rpc = Object.create(SDKRpcClientBase.prototype) as SDKRpcClientBase;
    const session = new Session({ id: 'ses_tower_unsupported', workDir: '/tmp/work', rpc });

    await expect(session.getTowerStatus()).rejects.toMatchObject({
      code: ErrorCodes.NOT_IMPLEMENTED,
    });
  });
});

describe('renderTowerStatusSummary', () => {
  it('renders the structured engine snapshot with upstream summary sections', () => {
    const status = {
      active: true,
      initialized: true,
      stateLost: false,
      state: {
        base: 'main',
        mode: 'branch',
        recoveredAt: null,
        missions: [
          {
            id: 'M1',
            title: 'Build engine',
            slug: 'build-engine',
            kind: 'build',
            scope: ['src/engine/**'],
            branch: 'feat/build-engine',
            worktree: 'wt-build',
            spawnBase: null,
            deps: [],
            status: 'planned',
            owner: null,
            context: null,
            tasks: [],
            notes: [],
            blockers: [],
          },
        ],
        roster: { agents: [] },
      },
      caller: 'tower',
      gate: [
        {
          missionId: 'M1',
          branch: 'feat/build-engine',
          status: 'planned',
          ready: false,
          noop: false,
          reasons: ['not-completed'],
          blocks: [{ reason: 'not-completed', message: 'merge blocked: mission is not completed' }],
          selectedReview: null,
          reviewBinding: null,
        },
      ],
      inbox: { count: 0 },
      concurrency: { budget: 2, inflight: 0, blockedUntil: null },
      recentActivity: ['tower init'],
    } satisfies Parameters<typeof renderTowerStatusSummary>[0];

    const summary = renderTowerStatusSummary(status);
    const [headline, ...bodyLines] = summary.split('\n');
    const body = bodyLines.join('\n');
    expect(headline).toBe('Tower status — ON');
    expect(body).toContain('Base: main (mode: branch) · You are: tower');
    expect(body).toContain('Missions:\n  M1 Build engine — planned · owner — · feat/build-engine');
    expect(body).toContain('Roster:\n  (none)');
    expect(body).toContain('Review gate:\n  M1 feat/build-engine — BLOCKED: not-completed');
    expect(body).toContain('Inbox: 0 message(s)');
    expect(body).toContain('Concurrency: budget: 2 agent(s) · inflight: 0 · spawns open');
    expect(body).toContain('Recent activity:\n  tower init');
  });

  it('keeps inactive and state-lost summaries explicit', () => {
    expect(
      renderTowerStatusSummary({ active: false, initialized: false, stateLost: false }),
    ).toBe('Tower mode: OFF');
    expect(
      renderTowerStatusSummary({ active: true, initialized: false, stateLost: false }),
    ).toBe('Tower mode: ON\nTower is not initialized.');
    const lost = renderTowerStatusSummary({ active: true, initialized: false, stateLost: true });
    expect(lost).toContain('Tower mode: ON');
    expect(lost).toContain('Tower state was lost');
    expect(lost).toContain('TowerSend/TowerComplete recover a minimal state');
  });
});
