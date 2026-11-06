import { describe, expect, it } from 'vitest';

import {
  v2SessionGroupPageSchema,
  v2SessionPageSchema,
  v2SessionsBatchRequestSchema,
  v2SessionsBatchResultSchema,
  v2SessionsListQuerySchema,
} from '../index';

// Wire fixtures mirror the fork's `route_sessions_v2.rs` json! emissions.
describe('v2 session REST schemas', () => {
  const item = {
    id: 'sess-1',
    workspace: { id: 'ws-1', cwd: 'G:/work' },
    meta: {
      title: 'A session',
      last_prompt: null,
      created_at: 1_700_000_000_000,
      updated_at: 1_700_000_100_000,
      archived: false,
      archived_at: null,
      has_prompt: true,
    },
    activity: { status: 'running', model: 'kimi-for-coding' },
  };

  it('round-trips the flat page', () => {
    const page = { items: [item], total: 1, has_more: false, next_page_token: null };
    expect(v2SessionPageSchema.parse(page)).toEqual(page);
  });

  it('round-trips the grouped page and the null-workspace corner', () => {
    const adHoc = { ...item, workspace: { id: null, cwd: null } };
    const page = {
      groups: [{ workspace: { id: 'ws-1', cwd: 'G:/work' }, sessions: [item], total: 1 }],
      total: 1,
      has_more: false,
      next_page_token: null,
    };
    expect(v2SessionGroupPageSchema.parse(page)).toEqual(page);
    expect(v2SessionPageSchema.parse({ items: [adHoc], total: 1, has_more: false, next_page_token: null })).toEqual({
      items: [adHoc],
      total: 1,
      has_more: false,
      next_page_token: null,
    });
  });

  it('accepts the full activity vocabulary', () => {
    for (const status of ['running', 'approval', 'question', 'failed', 'idle'] as const) {
      const withStatus = { ...item, activity: { status, model: null } };
      expect(
        v2SessionPageSchema.parse({ items: [withStatus], total: 1, has_more: false, next_page_token: null })
          .items[0]?.activity.status,
      ).toBe(status);
    }
  });

  it('round-trips the batch body and per-item result fold', () => {
    expect(v2SessionsBatchRequestSchema.parse({ ids: ['a', 'b'] })).toEqual({ ids: ['a', 'b'] });
    expect(v2SessionsBatchRequestSchema.safeParse({ ids: [] }).success).toBe(false);
    const result = {
      results: [
        { id: 'a', ok: true },
        { id: 'b', ok: false, error: 'session not found' },
      ],
      succeeded: 1,
      failed: 1,
    };
    expect(v2SessionsBatchResultSchema.parse(result)).toEqual(result);
  });

  it('declares only the query knobs the fork reads, tolerating the rest', () => {
    const query = v2SessionsListQuerySchema.parse({
      view: 'by_workspace',
      'group.page_size': '5',
      sort: 'meta.updated_at_desc', // accepted upstream knob the fork ignores
    });
    expect(query.view).toBe('by_workspace');
    expect(query['group.page_size']).toBe(5);
  });
});
