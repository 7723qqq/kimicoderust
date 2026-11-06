/**
 * The `/v2/sessions` family (kap-server `routes/v2/sessions.ts`): the
 * domain-grouped session list the Web sidebar loads, plus batch archive /
 * restore.
 *
 *   GET  /v2/sessions
 *     Query: meta.archived='true'|'false'  meta.has_prompt='true'|'false'
 *            view='by_workspace'           group.page_size=<n>
 *            (the server also accepts — and currently ignores — the upstream
 *            sort/page/page_token/workspace.id/activity.status/include/fields
 *            knobs; `meta.archived=all` is parsed as `false`, a recorded
 *            divergence from upstream's "no filter" reading)
 *     Reply: V2SessionPage (flat) | V2SessionGroupPage (view=by_workspace)
 *            — always a single complete page; `next_page_token` stays null
 *
 *   POST /v2/sessions:archive    body: V2SessionsBatchRequest
 *   POST /v2/sessions:restore    body: V2SessionsBatchRequest
 *     Reply: V2SessionsBatchResult — per-item outcomes; a missing session
 *            folds into its own item instead of failing the batch
 */

import { z } from 'zod';

/**
 * Upstream's activity vocabulary (`v2ActivityStatusSchema`). The fork's
 * server currently emits only `running` / `idle` (`route_sessions_v2.rs`);
 * the other values stay declared so a client written against the full
 * vocabulary still validates.
 */
export const v2ActivityStatusSchema = z.enum([
  'running',
  'approval',
  'question',
  'failed',
  'idle',
]);
export type V2ActivityStatus = z.infer<typeof v2ActivityStatusSchema>;

export const v2SessionWorkspaceSchema = z.object({
  // Nullable in the fork: ad-hoc-cwd sessions carry no workspace row, and the
  // grouped view keys them by their own id (upstream declares `id` non-null).
  id: z.string().nullable(),
  cwd: z.string().nullable(),
});
export type V2SessionWorkspace = z.infer<typeof v2SessionWorkspaceSchema>;

export const v2SessionMetaSchema = z.object({
  title: z.string().nullable(),
  // Never populated by the fork's server today (always null).
  last_prompt: z.string().nullable(),
  created_at: z.number().int(),
  updated_at: z.number().int(),
  archived: z.boolean(),
  archived_at: z.number().int().nullable(),
  // Fork addition the upstream meta does not carry: whether the session has
  // any prompt history (the `meta.has_prompt` query filter reads it).
  has_prompt: z.boolean(),
});
export type V2SessionMeta = z.infer<typeof v2SessionMetaSchema>;

export const v2SessionSchema = z.object({
  id: z.string(),
  workspace: v2SessionWorkspaceSchema,
  meta: v2SessionMetaSchema,
  activity: z.object({ status: v2ActivityStatusSchema, model: z.string().nullable() }),
  // Upstream's optional `git` domain (`include=git`) is declared for
  // cross-version tolerance; the fork's server never emits it.
  git: z
    .object({
      branch: z.string().nullable(),
      pull_request: z
        .object({
          number: z.number().int(),
          state: z.enum(['open', 'closed', 'merged']),
          url: z.string(),
        })
        .nullable(),
    })
    .optional(),
});
export type V2Session = z.infer<typeof v2SessionSchema>;

export const v2SessionPageSchema = z.object({
  items: z.array(v2SessionSchema),
  total: z.number().int(),
  has_more: z.boolean(),
  next_page_token: z.string().nullable(),
});
export type V2SessionPage = z.infer<typeof v2SessionPageSchema>;

export const v2SessionGroupSchema = z.object({
  workspace: v2SessionWorkspaceSchema,
  sessions: z.array(v2SessionSchema),
  total: z.number().int(),
});
export type V2SessionGroup = z.infer<typeof v2SessionGroupSchema>;

export const v2SessionGroupPageSchema = z.object({
  groups: z.array(v2SessionGroupSchema),
  total: z.number().int(),
  has_more: z.boolean(),
  next_page_token: z.string().nullable(),
});
export type V2SessionGroupPage = z.infer<typeof v2SessionGroupPageSchema>;

/** Query knobs the fork's server actually reads (see the header note for the
 *  accepted-but-ignored upstream superset). */
export const v2SessionsListQuerySchema = z
  .object({
    'meta.archived': z.enum(['true', 'false', 'all']).optional(),
    'meta.has_prompt': z.enum(['true', 'false']).optional(),
    view: z.enum(['flat', 'by_workspace']).optional(),
    'group.page_size': z.coerce.number().int().min(1).optional(),
  })
  .passthrough();
export type V2SessionsListQuery = z.infer<typeof v2SessionsListQuerySchema>;

/** Batch body for `:archive` / `:restore`. The server rejects >5000 ids with
 *  a 422 envelope (`validation.failed`). */
export const v2SessionsBatchRequestSchema = z.object({
  ids: z.array(z.string().min(1)).min(1),
});
export type V2SessionsBatchRequest = z.infer<typeof v2SessionsBatchRequestSchema>;

export const v2SessionsBatchResultSchema = z.object({
  results: z.array(
    z.object({
      id: z.string(),
      ok: z.boolean(),
      // Recorded fork shape: a bare string. Upstream's
      // `v2SessionsBatchResultSchema` carries `{ code, message }` instead —
      // the divergence is tracked, not reconciled here.
      error: z.string().optional(),
    }),
  ),
  succeeded: z.number().int(),
  failed: z.number().int(),
});
export type V2SessionsBatchResult = z.infer<typeof v2SessionsBatchResultSchema>;
