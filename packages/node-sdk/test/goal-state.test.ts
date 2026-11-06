import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  goalStateFromEngineStore,
  goalStateFromSnapshot,
  nativeGoalWallClockMs,
  toGoalSnapshot,
  type NativeGoalState,
} from '#/native/sdk-client/helpers';

const NOW = Date.UTC(2026, 0, 1, 0, 0, 0);

function goal(overrides: Partial<NativeGoalState> = {}): NativeGoalState {
  return {
    goalId: 'goal-1',
    objective: 'Ship the feature',
    status: 'active',
    createdAt: NOW - 60_000,
    updatedAt: NOW - 60_000,
    turnsUsed: 3,
    tokensUsed: 100,
    inputTokensUsed: 60,
    outputTokensUsed: 40,
    wallClockMs: 42_000,
    budget: { tokenBudget: 500, turnBudget: 10, wallClockBudgetMs: 3_600_000 },
    ...overrides,
  };
}

afterEach(() => {
  vi.useRealTimers();
});

describe('goal snapshots', () => {
  it('adopts the budget limits and wall clock the engine reports', () => {
    const state = goalStateFromSnapshot({
      goalId: 'goal-1',
      objective: 'Ship the feature',
      status: 'active',
      turnsUsed: 2,
      tokensUsed: 90,
      inputTokensUsed: 50,
      outputTokensUsed: 40,
      wallClockMs: 12_345,
      createdAt: NOW - 10_000,
      updatedAt: NOW - 5_000,
      budget: {
        tokenBudget: 500,
        turnBudget: 10,
        wallClockBudgetMs: 60_000,
        remainingTokens: 410,
        remainingTurns: 8,
        remainingWallClockMs: 47_655,
        tokenBudgetReached: false,
        turnBudgetReached: false,
        wallClockBudgetReached: false,
        overBudget: false,
        inputTokensUsed: 50,
        outputTokensUsed: 40,
      },
    });
    expect(state.budget).toEqual({
      tokenBudget: 500,
      turnBudget: 10,
      wallClockBudgetMs: 60_000,
    });
    expect(state.wallClockMs).toBe(12_345);
    expect(state.turnsUsed).toBe(2);
  });

  it('fills the v2 budget report, including reached flags', () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const snapshot = toGoalSnapshot(
      goal({
        wallClockMs: 3_400_000,
        updatedAt: NOW,
        tokensUsed: 600,
        turnsUsed: 10,
      }),
    );
    expect(snapshot.budget).toMatchObject({
      tokenBudget: 500,
      turnBudget: 10,
      wallClockBudgetMs: 3_600_000,
      remainingTokens: 0,
      remainingTurns: 0,
      remainingWallClockMs: 200_000,
      tokenBudgetReached: true,
      turnBudgetReached: true,
      wallClockBudgetReached: false,
      overBudget: true,
      inputTokensUsed: 60,
      outputTokensUsed: 40,
    });
  });

  it('leaves absent limits null and unbounded', () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const snapshot = toGoalSnapshot(goal({ budget: {} }));
    expect(snapshot.budget).toMatchObject({
      tokenBudget: null,
      turnBudget: null,
      wallClockBudgetMs: null,
      remainingTokens: null,
      remainingTurns: null,
      remainingWallClockMs: null,
      overBudget: false,
    });
  });

  it('keeps the live wall clock running only while the goal is active', () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    // The engine reported 42s at `updatedAt` a minute ago.
    expect(nativeGoalWallClockMs(goal(), NOW)).toBe(102_000);
    vi.setSystemTime(NOW + 30_000);
    expect(nativeGoalWallClockMs(goal(), NOW + 30_000)).toBe(132_000);
    expect(
      nativeGoalWallClockMs(goal({ status: 'paused' }), NOW + 30_000),
    ).toBe(42_000);
  });

  it('falls back to the goal age for host-created goals without an engine reading', () => {
    expect(
      nativeGoalWallClockMs(goal({ wallClockMs: undefined }), NOW),
    ).toBe(60_000);
  });
});

describe('engine-store goal decoding', () => {
  it('keeps the stop reason and the criterion the engine reported', () => {
    const state = goalStateFromSnapshot({
      goalId: 'goal-1',
      objective: 'Ship the feature',
      completionCriterion: 'Tests pass',
      status: 'paused',
      turnsUsed: 1,
      tokensUsed: 1,
      terminalReason: 'Paused by the user',
      createdAt: NOW,
      updatedAt: NOW,
    });
    expect(state.terminalReason).toBe('Paused by the user');
    expect(state.completionCriterion).toBe('Tests pass');
    expect(toGoalSnapshot(state)).toMatchObject({
      terminalReason: 'Paused by the user',
      completionCriterion: 'Tests pass',
    });
  });

  it('decodes the durable state the store keeps', () => {
    const state = goalStateFromEngineStore(
      JSON.stringify({
        goal: {
          goalId: 'goal-1',
          objective: 'Ship the feature',
          completionCriterion: 'Tests pass',
          status: 'paused',
          turnsUsed: 2,
          tokensUsed: 90,
          inputTokensUsed: 50,
          outputTokensUsed: 40,
          wallClockMs: 12_345,
          wallClockResumedAt: null,
          budgetLimits: { tokenBudget: 500, turnBudget: 10, wallClockBudgetMs: 60_000 },
          terminalReason: 'Paused by the user',
          hostOwned: true,
          createdAt: NOW - 10_000,
          updatedAt: NOW - 5_000,
        },
      }),
    );
    // A paused goal is settled: its elapsed figure is final, and `updatedAt`
    // stays the engine's own stamp.
    expect(state).toMatchObject({
      goalId: 'goal-1',
      completionCriterion: 'Tests pass',
      status: 'paused',
      turnsUsed: 2,
      wallClockMs: 12_345,
      updatedAt: NOW - 5_000,
      terminalReason: 'Paused by the user',
      budget: { tokenBudget: 500, turnBudget: 10, wallClockBudgetMs: 60_000 },
    });
  });

  it('rebases a running goal onto now from its resume stamp', () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const state = goalStateFromEngineStore(
      JSON.stringify({
        goal: {
          goalId: 'goal-1',
          objective: 'Ship the feature',
          status: 'active',
          turnsUsed: 0,
          tokensUsed: 0,
          // Settled 5s at NOW-3s: the engine's live figure is 8s.
          wallClockMs: 5_000,
          wallClockResumedAt: NOW - 3_000,
          createdAt: NOW - 60_000,
          updatedAt: NOW - 40_000,
        },
      }),
    )!;
    expect(state.wallClockMs).toBe(8_000);
    expect(state.updatedAt).toBe(NOW);
    expect(nativeGoalWallClockMs(state, NOW)).toBe(8_000);
    expect(nativeGoalWallClockMs(state, NOW + 2_000)).toBe(10_000);
  });

  it('distinguishes an unreadable payload from a stored "no goal"', () => {
    expect(goalStateFromEngineStore('{"goal":null}')).toBeNull();
    expect(goalStateFromEngineStore(null)).toBeUndefined();
    expect(goalStateFromEngineStore('not json')).toBeUndefined();
    expect(goalStateFromEngineStore('[]')).toBeUndefined();
    expect(goalStateFromEngineStore('{"todo":[]}')).toBeUndefined();
  });
});
