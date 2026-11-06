import { createRequire } from 'node:module';
import { resolve } from 'node:path';

import { findKimiAgentAddon } from '@moonshot-ai/kimi-agent/session-handle';

interface JsTowerStatusMissionTask {
  readonly text: string;
  readonly done: boolean;
  readonly dropped: boolean;
}

interface JsTowerStatusMission {
  readonly id: string;
  readonly title: string;
  readonly slug: string;
  readonly kind: string;
  readonly scope: readonly string[];
  readonly branch: string;
  readonly worktree: string;
  readonly spawnBase?: string | null;
  readonly deps: readonly string[];
  readonly status: string;
  readonly owner?: string | null;
  readonly context?: string | null;
  readonly tasks: readonly JsTowerStatusMissionTask[];
  readonly notes: readonly string[];
  readonly blockers: readonly string[];
}

interface JsTowerStatusRosterEntry {
  readonly name: string;
  readonly agentId: string;
  readonly sessionId?: string | null;
  readonly kind: string;
  readonly missionId?: string | null;
  readonly reviewTarget?: string | null;
  readonly reviewMissionId?: string | null;
  readonly worktree?: string | null;
  readonly branch?: string | null;
  readonly spawnedAt: string;
  readonly diedAt?: string | null;
  readonly deathStatus?: string | null;
  readonly deathReason?: string | null;
  readonly status?: string | null;
}

interface JsTowerStatusState {
  readonly base: string;
  readonly mode: string;
  readonly recoveredAt?: string | null;
  readonly missions: readonly JsTowerStatusMission[];
  readonly roster: { readonly agents: readonly JsTowerStatusRosterEntry[] };
}

interface JsTowerStatusReview {
  readonly reviewer: string;
  readonly target: string;
  readonly round: number;
  readonly status: string;
  readonly merge: string;
  readonly reviewedCommit: string;
  readonly date: string;
  readonly file: string;
  readonly mission?: string | null;
  readonly seq?: number | null;
  readonly modifiedAtMs: number;
}

interface JsTowerStatusGate {
  readonly missionId: string;
  readonly branch: string;
  readonly status: string;
  readonly ready: boolean;
  readonly noop: boolean;
  readonly reasons: readonly string[];
  readonly blocks: readonly { readonly reason: string; readonly message: string }[];
  readonly selectedReview?: JsTowerStatusReview | null;
  readonly reviewBinding?: string | null;
}

interface JsTowerStatusConcurrency {
  readonly budget: number;
  readonly inflight: number;
  readonly blockedUntil?: number | null;
}

export interface JsTowerStatus {
  readonly active: boolean;
  readonly initialized: boolean;
  readonly stateLost: boolean;
  readonly state?: JsTowerStatusState | null;
  readonly caller?: string | null;
  readonly gate?: readonly JsTowerStatusGate[] | null;
  readonly inbox?: { readonly count: number } | null;
  readonly concurrency?: JsTowerStatusConcurrency | null;
  readonly recentActivity?: readonly string[] | null;
}

interface TowerStatusNativeModule {
  sessionTowerStatus(
    sessionId: string,
    active: boolean,
    maxRunningTasks?: number,
  ): Promise<JsTowerStatus>;
}

const require = createRequire(import.meta.url);

/** Load the raw session addon; session exports do not pass through the tools wrapper. */
export async function readNativeTowerStatus(
  sessionId: string,
  active: boolean,
  maxRunningTasks?: number,
): Promise<JsTowerStatus> {
  const modulePath = findKimiAgentAddon();
  if (modulePath === null) {
    throw new Error('kimi_agent native addon not built; run `napi build` in packages/kimi-agent');
  }
  const native = require(resolve(modulePath)) as TowerStatusNativeModule;
  return native.sessionTowerStatus(sessionId, active, maxRunningTasks);
}

function summaryMissions(missions: readonly JsTowerStatusMission[]): string[] {
  if (missions.length === 0) return ['  (none)'];
  return missions.map(
    (mission) =>
      `  ${mission.id} ${mission.title} — ${mission.status} · owner ${mission.owner ?? '—'} · ${mission.branch}`,
  );
}

function summaryRecoveredAt(state: JsTowerStatusState): string[] {
  if (state.recoveredAt === undefined || state.recoveredAt === null) return [];
  return [
    `Warning: State recovered at ${state.recoveredAt} after a state loss — all mission, roster, and review history from before that point is gone; report this history loss to the user instead of continuing as if nothing happened.`,
  ];
}

function summaryRoster(agents: readonly JsTowerStatusRosterEntry[]): string[] {
  if (agents.length === 0) return ['  (none)'];
  return agents.map((agent) => {
    const assignment =
      agent.kind === 'worker'
        ? `mission ${agent.missionId ?? '?'}`
        : `reviewing ${agent.reviewTarget ?? '?'}`;
    const death = agent.diedAt === undefined || agent.diedAt === null
      ? ''
      : ` · died ${agent.deathStatus ?? 'unknown'}`;
    return `  ${agent.name} (${agent.kind}) — ${assignment} · ${agent.agentId}${death}`;
  });
}

function summaryGate(gate: readonly JsTowerStatusGate[]): string[] {
  if (gate.length === 0) return ['  (no open missions)'];
  return gate.map((missionGate) => {
    const review = missionGate.selectedReview;
    const reviewText = review === undefined || review === null
      ? ''
      : ` · review round ${review.round} by ${review.reviewer} ${review.status}/${review.merge || 'legacy'} · binding ${missionGate.reviewBinding ?? 'unavailable'}`;
    if (missionGate.ready) {
      return `  ${missionGate.missionId} ${missionGate.branch} — READY${missionGate.noop ? ' (zero-diff no-op)' : ''}${reviewText}`;
    }
    const reasons = missionGate.blocks.map((block) => block.reason).join(', ');
    return `  ${missionGate.missionId} ${missionGate.branch} — BLOCKED: ${reasons}${reviewText}`;
  });
}

function summaryConcurrency(snapshot: JsTowerStatusConcurrency): string {
  const base = `budget: ${snapshot.budget} agent(s) · inflight: ${snapshot.inflight}`;
  if (snapshot.blockedUntil === undefined || snapshot.blockedUntil === null) {
    return `${base} · spawns open`;
  }
  const remainingMs = snapshot.blockedUntil - Date.now();
  return remainingMs > 0
    ? `${base} · spawns paused for ~${Math.ceil(remainingMs / 1000)}s`
    : `${base} · spawn pause expired`;
}

/** Upstream `readTowerStatusSummary`: deterministic presentation of the NAPI snapshot. */
export function renderTowerStatusSummary(status: JsTowerStatus): string {
  if (!status.initialized) {
    if (status.stateLost) {
      return status.active
        ? 'Tower mode: ON\nTower state was lost: .tower/comms/state.json is missing but the .tower/ directory still exists — all recorded mission, roster, and review history is gone. Report this history loss to the user; TowerSend/TowerComplete recover a minimal state (stamped recoveredAt) automatically.'
        : 'Tower mode: OFF';
    }
    return status.active ? 'Tower mode: ON\nTower is not initialized.' : 'Tower mode: OFF';
  }
  if (
    status.state === undefined || status.state === null ||
    status.caller === undefined || status.caller === null ||
    status.inbox === undefined || status.inbox === null ||
    status.concurrency === undefined || status.concurrency === null
  ) {
    throw new Error('initialized tower status is incomplete');
  }
  const { state, caller, inbox, concurrency } = status;
  const gate = status.gate ?? [];
  const recentActivity = status.recentActivity ?? [];
  const lines = [
    status.active ? 'Tower status — ON' : 'Tower mode: OFF',
    `Base: ${state.base} (mode: ${state.mode}) · You are: ${caller}`,
    ...summaryRecoveredAt(state),
    'Missions:',
    ...summaryMissions(state.missions),
    'Roster:',
    ...summaryRoster(state.roster.agents),
    'Review gate:',
    ...summaryGate(gate),
    `Inbox: ${inbox.count} message(s)`,
    `Concurrency: ${summaryConcurrency(concurrency)}`,
    'Recent activity:',
    ...(recentActivity.length > 0 ? recentActivity.map((line) => `  ${line}`) : ['  (empty)']),
  ];
  return lines.join('\n');
}
