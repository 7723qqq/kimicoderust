/**
 *   GET  /v1/plugins/marketplace        -                     data: PluginMarketplaceResponse
 *   GET  /v1/plugins                    -                     data: ListPluginsResponse
 *   POST /v1/plugins                    body: InstallPluginRequest
 *                                                             data: InstallPluginResponse
 *   POST /v1/plugins/{id}:enable        -                     data: PluginSetEnabledResponse
 *   POST /v1/plugins/{id}:disable       -                     data: PluginSetEnabledResponse
 *   POST /v1/plugins/{id}:remove        -                     data: PluginRemoveResponse
 *
 * Shapes verified against the fork's handlers
 * (`kimi-agent/src/server/route_plugins_oauth.rs` + `plugins.rs`); the schema
 * skeleton is ported from kap-server `protocol/rest-plugin.ts` where the fork
 * agrees with it.
 */

import { z } from 'zod';

/** v2 `PluginGithubMetadata`. `installedSha` is deliberately absent: the fork
 *  records no install-time commit, and a value there would be invented. */
export const pluginGithubMetadataSchema = z.object({
  owner: z.string(),
  repo: z.string(),
  ref: z.object({
    kind: z.enum(['branch', 'tag', 'sha']),
    value: z.string(),
  }),
});
export type PluginGithubMetadataWire = z.infer<typeof pluginGithubMetadataSchema>;

/** v2 `PluginSource` vocabulary (`local-path` | `zip-url` | `github`). */
export const pluginSourceKindSchema = z.enum(['local-path', 'zip-url', 'github']);
export type PluginSourceKind = z.infer<typeof pluginSourceKindSchema>;

/**
 * One installed plugin in the `/v1/plugins` list. Verified against the fork's
 * `PluginSummary`; two deliberate differences from kap-server's
 * `pluginSummarySchema`, kept as-is rather than reconciled here:
 *   - the display label rides `name` (upstream declares `displayName`);
 *   - `state` adds the fork-only `remote` value (catalogued but not on disk)
 *     on top of `ok` / `error`.
 */
export const pluginSummarySchema = z.object({
  id: z.string(),
  name: z.string(),
  version: z.string(),
  enabled: z.boolean(),
  description: z.string(),
  source: pluginSourceKindSchema,
  originalSource: z.string().optional(),
  github: pluginGithubMetadataSchema.optional(),
  state: z.enum(['ok', 'error', 'remote']),
  skillCount: z.number().int().nonnegative(),
  mcpServerCount: z.number().int().nonnegative(),
  enabledMcpServerCount: z.number().int().nonnegative(),
  commandCount: z.number().int().nonnegative(),
  hookCount: z.number().int().nonnegative(),
  hasErrors: z.boolean(),
});
export type PluginSummaryWire = z.infer<typeof pluginSummarySchema>;

export const listPluginsResponseSchema = z.object({
  plugins: z.array(pluginSummarySchema),
});
export type ListPluginsResponse = z.infer<typeof listPluginsResponseSchema>;

/** The recorded install state embedded in a marketplace entry (the fork's
 *  `InstalledPluginInfo`, camelCase on the wire). */
export const installedPluginInfoSchema = z.object({
  enabled: z.boolean(),
  version: z.string().optional(),
  root: z.string().optional(),
  installedAt: z.string().optional(),
  originalSource: z.string().optional(),
  updatedAt: z.string().optional(),
});
export type InstalledPluginInfoWire = z.infer<typeof installedPluginInfoSchema>;

/**
 * One `/v1/plugins/marketplace` entry (the fork's `PluginMarketplaceEntry`).
 * `tier` is a free-form string (`official` / `curated` observed; upstream
 * types it as the enum `['official', 'curated', 'third-party']`), and
 * `installed` carries the full recorded install — not upstream's
 * `{ version?, enabled }` projection. Upstream's `updateAvailable` /
 * `capabilityId` have no fork producer.
 */
export const pluginMarketplaceEntrySchema = z.object({
  id: z.string(),
  tier: z.string(),
  displayName: z.string(),
  description: z.string(),
  homepage: z.string().optional(),
  keywords: z.array(z.string()).optional(),
  source: z.string(),
  version: z.string().optional(),
  installed: installedPluginInfoSchema.optional(),
});
export type PluginMarketplaceEntryWire = z.infer<typeof pluginMarketplaceEntrySchema>;

export const pluginMarketplaceResponseSchema = z.object({
  entries: z.array(pluginMarketplaceEntrySchema),
});
export type PluginMarketplaceResponse = z.infer<typeof pluginMarketplaceResponseSchema>;

/**
 * `POST /v1/plugins` body: install from a catalog id, a catalog `source`, a
 * local root, or a remote archive URL. The server reads the first string
 * among `id` / `name` / `source` (`source` is what the official Web bundle's
 * `installPlugin` posts).
 */
export const installPluginRequestSchema = z
  .object({
    id: z.string().min(1).optional(),
    name: z.string().min(1).optional(),
    source: z.string().min(1).optional(),
  })
  .refine((value) => value.id !== undefined || value.name !== undefined || value.source !== undefined, {
    message: 'one of id, name, or source is required',
  });
export type InstallPluginRequest = z.infer<typeof installPluginRequestSchema>;

export const installPluginResponseSchema = z.object({
  id: z.string(),
  enabled: z.boolean(),
  version: z.string().nullable(),
  installed: z.literal(true),
});
export type InstallPluginResponse = z.infer<typeof installPluginResponseSchema>;

export const pluginSetEnabledResponseSchema = z.object({
  ok: z.literal(true),
  pluginId: z.string(),
  enabled: z.boolean(),
});
export type PluginSetEnabledResponse = z.infer<typeof pluginSetEnabledResponseSchema>;

export const pluginRemoveResponseSchema = z.object({
  ok: z.literal(true),
  pluginId: z.string(),
  removed: z.literal(true),
});
export type PluginRemoveResponse = z.infer<typeof pluginRemoveResponseSchema>;
