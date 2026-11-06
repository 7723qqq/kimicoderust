/**
 *   GET    /v1/models                    -     data: ListModelsResponse
 *                                              (alias: GET /v1/model-catalog)
 *   GET    /v1/providers                 -     data: ListProvidersResponse
 *   POST   /v1/providers                 body: CreateProviderRequest
 *                                        data: CreateProviderResponse (HTTP 201)
 *   GET    /v1/providers/{id}            -     data: GetProviderResponse
 *   PUT    /v1/providers/{id}            body: ReplaceProviderRequest
 *                                        data: ReplaceProviderResponse
 *   DELETE /v1/providers/{id}            -     data: DeleteProviderResponse
 *   POST   /v1/providers:refresh         -     data: RefreshProviderModelsResponse
 *   POST   /v1/providers/{id}:refresh    -     data: RefreshProviderModelsResponse
 *   POST   /v1/providers:refresh_oauth   -     data: RefreshOAuthProviderModelsResponse
 *   GET    /v1/catalog/providers         -     data: ListCatalogProvidersResponse
 *                                              (alias: GET /v1/providers/catalog)
 *   GET    /v1/catalog/providers/{id}    -     data: GetCatalogProviderResponse
 *   POST   /v1/providers:import_catalog  body: ImportCatalogProviderRequest
 *                                        data: ImportCatalogProviderResponse (HTTP 201)
 *   POST   /v1/providers:import_registry body: ImportRegistryProviderRequest
 *                                        data: ImportCustomRegistryResponse (HTTP 201)
 *   POST   /v1/models/{id}:set_default   -     sets the global default model
 *                                        data: SetDefaultModelResponse
 *                                        (recorded drift: the fork's server
 *                                        actually returns `{ model: string }`
 *                                        with no alias existence check)
 */

import { z } from 'zod';

import {
  modelCatalogItemSchema,
  providerCatalogItemSchema,
  providerRefreshChangeSchema,
  providerRefreshFailureSchema,
} from '../modelCatalog';

export const listModelsResponseSchema = z.object({
  items: z.array(modelCatalogItemSchema),
});
export type ListModelsResponse = z.infer<typeof listModelsResponseSchema>;

export const listProvidersResponseSchema = z.object({
  items: z.array(providerCatalogItemSchema),
});
export type ListProvidersResponse = z.infer<typeof listProvidersResponseSchema>;

// Unlike the list route, `GET /providers/{id}` reveals the stored `api_key`
// so clients can prefill an edit form (kap-server
// `rest-modelCatalog.ts:getProviderResponseSchema`; the fork's Rust handler
// attaches it the same way).
export const getProviderResponseSchema = providerCatalogItemSchema.extend({
  api_key: z.string().optional(),
});
export type GetProviderResponse = z.infer<typeof getProviderResponseSchema>;

export const setDefaultModelResponseSchema = z.object({
  default_model: z.string().min(1),
  model: modelCatalogItemSchema,
});
export type SetDefaultModelResponse = z.infer<typeof setDefaultModelResponseSchema>;

export const refreshOAuthProviderModelsResponseSchema = z.object({
  changed: z.array(providerRefreshChangeSchema),
  unchanged: z.array(z.string().min(1)),
  failed: z.array(providerRefreshFailureSchema),
});
export type RefreshOAuthProviderModelsResponse = z.infer<
  typeof refreshOAuthProviderModelsResponseSchema
>;

export const refreshProviderModelsResponseSchema = z.object({
  changed: z.array(providerRefreshChangeSchema),
  unchanged: z.array(z.string().min(1)),
  failed: z.array(providerRefreshFailureSchema),
});
export type RefreshProviderModelsResponse = z.infer<
  typeof refreshProviderModelsResponseSchema
>;

// ── Provider write routes ───────────────────────────────────────────────────
// Ported from kap-server `protocol/rest-modelCatalog.ts` and verified against
// the fork's handlers (`kimi-agent/src/server/provider_write.rs`,
// `catalog_routes.rs`).

/** The six wire types the server's write validator accepts (same order). */
export const providerWireTypeSchema = z.enum([
  'kimi',
  'openai',
  'openai_responses',
  'anthropic',
  'google-genai',
  'vertexai',
]);
export type ProviderWireType = z.infer<typeof providerWireTypeSchema>;

/**
 * A provider id the write routes accept: upstream `PROVIDER_ID_PATTERN`
 * (`^[\p{L}\p{N}][\p{L}\p{N}\-_ ]*$` — a letter or digit first, then letters,
 * digits, `-`, `_` or spaces; unicode allowed). The Rust server enforces the
 * same rule in `provider_write.rs::validate_provider_id`.
 *
 * Engine-generated OAuth provider ids carry the internal `managed:` prefix
 * (today only `managed:kimi-code`, provisioned by the login flow into
 * config.toml). They are never submitted through these routes, deliberately
 * fail this schema (the colon), and load fine because config reads do not
 * validate provider ids.
 */
export const providerIdSchema = z
  .string()
  .regex(
    /^[\p{L}\p{N}][\p{L}\p{N}\-_ ]*$/u,
    'id must start with a letter or digit and may only contain letters, digits, "-", "_" and spaces',
  );
export type ProviderId = z.infer<typeof providerIdSchema>;

/** One `models[]` entry on a provider write (kap-server `createProviderModelSchema`). */
export const createProviderModelSchema = z.object({
  model: z.string().min(1),
  max_context_size: z.number().int().min(1),
  display_name: z.string().min(1).optional(),
  capabilities: z.array(z.string()).optional(),
  max_output_size: z.number().int().min(1).optional(),
  support_efforts: z.array(z.string().min(1)).optional(),
  adaptive_thinking: z.boolean().optional(),
});
export type CreateProviderModel = z.infer<typeof createProviderModelSchema>;

function refineProviderForm(
  value: { base_url?: string | undefined; models: ReadonlyArray<{ model: string }> },
  ctx: z.RefinementCtx,
): void {
  if (value.base_url !== undefined && value.base_url.includes('${')) {
    ctx.addIssue({
      code: 'custom',
      message: 'base_url must not contain an environment variable placeholder',
      path: ['base_url'],
    });
  }
  const seen = new Set<string>();
  for (const entry of value.models) {
    if (seen.has(entry.model)) {
      ctx.addIssue({
        code: 'custom',
        message: `duplicate model: ${entry.model}`,
        path: ['models'],
      });
      return;
    }
    seen.add(entry.model);
  }
}

function refineDefaultModelMembership(
  value: { default_model?: string | undefined; models: ReadonlyArray<{ model: string }> },
  ctx: z.RefinementCtx,
): void {
  if (
    value.default_model !== undefined &&
    !value.models.some((entry) => entry.model === value.default_model)
  ) {
    ctx.addIssue({
      code: 'custom',
      message: 'default_model must be one of models[].model',
      path: ['default_model'],
    });
  }
}

// Upstream's `api_key_env` is deliberately absent: the fork's
// `CreateProviderForm` does not model it, so the server silently drops the key
// on write (tracked as a conflict item, not declared here).
export const createProviderRequestSchema = z
  .object({
    id: providerIdSchema,
    type: providerWireTypeSchema,
    api_key: z.string().optional(),
    base_url: z.string().trim().optional(),
    default_model: z.string().min(1).optional(),
    models: z.array(createProviderModelSchema).min(1),
  })
  .superRefine((value, ctx) => {
    refineProviderForm(value, ctx);
    refineDefaultModelMembership(value, ctx);
  });
export type CreateProviderRequest = z.infer<typeof createProviderRequestSchema>;

export const createProviderResponseSchema = providerCatalogItemSchema;
export type CreateProviderResponse = z.infer<typeof createProviderResponseSchema>;

export const replaceProviderRequestSchema = z
  .object({
    new_id: providerIdSchema.optional(),
    type: providerWireTypeSchema,
    api_key: z.string().optional(),
    base_url: z.string().trim().optional(),
    default_model: z.string().min(1).optional(),
    models: z.array(createProviderModelSchema).min(1),
  })
  .superRefine((value, ctx) => {
    refineProviderForm(value, ctx);
    refineDefaultModelMembership(value, ctx);
  });
export type ReplaceProviderRequest = z.infer<typeof replaceProviderRequestSchema>;

export const replaceProviderResponseSchema = z.object({
  provider: providerCatalogItemSchema,
});
export type ReplaceProviderResponse = z.infer<typeof replaceProviderResponseSchema>;

export const deleteProviderResponseSchema = z.object({
  deleted: z.literal(true),
});
export type DeleteProviderResponse = z.infer<typeof deleteProviderResponseSchema>;

// ── Catalog browse (models.dev directory) ───────────────────────────────────

export const catalogModelItemSchema = z.object({
  id: z.string().min(1),
  name: z.string().optional(),
  max_context_size: z.number().int().min(1),
  capabilities: z.array(z.string()).optional(),
  reasoning: z.boolean(),
});
export type CatalogModelItem = z.infer<typeof catalogModelItemSchema>;

export const catalogProviderItemSchema = z.object({
  id: z.string().min(1),
  name: z.string().min(1),
  wire_type: providerWireTypeSchema.nullable(),
  base_url: z.string().nullable(),
  guessed: z.boolean(),
  needs_base_url: z.boolean(),
  rejected: z.boolean(),
  reject_reason: z.string().nullable(),
  env_key: z.string().nullable(),
  models: z.array(catalogModelItemSchema),
});
export type CatalogProviderItem = z.infer<typeof catalogProviderItemSchema>;

export const listCatalogProvidersResponseSchema = z.object({
  items: z.array(catalogProviderItemSchema),
});
export type ListCatalogProvidersResponse = z.infer<typeof listCatalogProvidersResponseSchema>;

export const getCatalogProviderResponseSchema = catalogProviderItemSchema;
export type GetCatalogProviderResponse = z.infer<typeof getCatalogProviderResponseSchema>;

// ── Catalog / registry import ───────────────────────────────────────────────

// The fork's importer additionally accepts `api_key_env` (resolved at request
// time instead of storing the key) — kap-server's
// `providerCollectionActionBodySchema` has no such field.
export const importCatalogProviderRequestSchema = z.object({
  catalog_id: z.string().min(1),
  id: providerIdSchema.optional(),
  base_url: z.string().optional(),
  api_key: z.string().optional(),
  api_key_env: z.string().optional(),
});
export type ImportCatalogProviderRequest = z.infer<typeof importCatalogProviderRequestSchema>;

export const importCatalogProviderResponseSchema = z.object({
  provider: providerCatalogItemSchema,
  models_imported: z.number().int().min(0),
});
export type ImportCatalogProviderResponse = z.infer<
  typeof importCatalogProviderResponseSchema
>;

export const importRegistryProviderRequestSchema = z.object({
  url: z.string().min(1),
  api_key: z.string().optional(),
});
export type ImportRegistryProviderRequest = z.infer<typeof importRegistryProviderRequestSchema>;

export const importCustomRegistryResponseSchema = z.object({
  providers: z.array(providerCatalogItemSchema),
  models_imported: z.number().int().min(0),
  credential_env: z.record(z.string(), z.string()).optional(),
});
export type ImportCustomRegistryResponse = z.infer<typeof importCustomRegistryResponseSchema>;
