import { describe, expect, it } from 'vitest';

import {
  createProviderRequestSchema,
  deleteProviderResponseSchema,
  getCatalogProviderResponseSchema,
  getProviderResponseSchema,
  importCatalogProviderRequestSchema,
  importCustomRegistryResponseSchema,
  importRegistryProviderRequestSchema,
  listCatalogProvidersResponseSchema,
  listModelsResponseSchema,
  listProvidersResponseSchema,
  modelCatalogItemSchema,
  providerCatalogItemSchema,
  providerCatalogStatusSchema,
  providerWireTypeSchema,
  replaceProviderRequestSchema,
  replaceProviderResponseSchema,
  setDefaultModelResponseSchema,
  type ModelCatalogItem,
  type ProviderCatalogItem,
} from '../index';

describe('model catalog schemas', () => {
  const model: ModelCatalogItem = {
    provider: 'kimi',
    model: 'k2',
    display_name: 'Kimi K2',
    max_context_size: 131072,
    capabilities: ['thinking'],
  };

  const provider: ProviderCatalogItem = {
    id: 'kimi',
    type: 'kimi',
    base_url: 'https://api.example.test/v1',
    default_model: 'k2',
    has_api_key: true,
    status: 'connected',
    models: ['k2'],
  };

  it('round-trips a model catalog item', () => {
    expect(modelCatalogItemSchema.parse(model)).toEqual(model);
  });

  it('rejects invalid model context sizes', () => {
    expect(
      modelCatalogItemSchema.safeParse({ ...model, max_context_size: 0 }).success,
    ).toBe(false);
  });

  it.each(['connected', 'error', 'unconfigured'] as const)(
    'accepts provider status %s',
    (status) => {
      expect(providerCatalogStatusSchema.parse(status)).toBe(status);
    },
  );

  it('round-trips a provider catalog item', () => {
    expect(providerCatalogItemSchema.parse(provider)).toEqual(provider);
    expect(getProviderResponseSchema.parse(provider)).toEqual(provider);
  });

  it('round-trips list responses and set-default response', () => {
    expect(listModelsResponseSchema.parse({ items: [model] })).toEqual({
      items: [model],
    });
    expect(listProvidersResponseSchema.parse({ items: [provider] })).toEqual({
      items: [provider],
    });
    expect(
      setDefaultModelResponseSchema.parse({ default_model: 'k2', model }),
    ).toEqual({
      default_model: 'k2',
      model,
    });
  });

  it('reveals api_key on the single-provider read only', () => {
    expect(getProviderResponseSchema.parse({ ...provider, api_key: 'sk-x' })).toEqual({
      ...provider,
      api_key: 'sk-x',
    });
    // The list item schema stays credential-free.
    expect(providerCatalogItemSchema.parse(provider)).toEqual(provider);
  });

  describe('provider write routes', () => {
    const createBody = {
      id: 'acme',
      type: 'openai',
      api_key: 'sk-x',
      base_url: 'https://api.example.test/v1',
      default_model: 'k3',
      models: [{ model: 'k3', max_context_size: 200_000 }],
    };

    it('accepts a minimal create body and applies the server-side refinements', () => {
      expect(createProviderRequestSchema.parse(createBody)).toEqual(createBody);
      // base_url env placeholder
      expect(
        createProviderRequestSchema.safeParse({
          ...createBody,
          base_url: 'https://${HOST}/v1',
        }).success,
      ).toBe(false);
      // duplicate model
      expect(
        createProviderRequestSchema.safeParse({
          ...createBody,
          models: [
            { model: 'k3', max_context_size: 1 },
            { model: 'k3', max_context_size: 2 },
          ],
        }).success,
      ).toBe(false);
      // default_model outside models[]
      expect(
        createProviderRequestSchema.safeParse({
          ...createBody,
          default_model: 'other',
        }).success,
      ).toBe(false);
    });

    it('rejects a wire type the server validator would reject', () => {
      expect(providerWireTypeSchema.safeParse('exotic').success).toBe(false);
      expect(
        createProviderRequestSchema.safeParse({ ...createBody, type: 'exotic' }).success,
      ).toBe(false);
    });

    it('applies the upstream PROVIDER_ID_PATTERN like the Rust server does', () => {
      // The same boundary matrix as
      // `provider_write.rs::provider_id_follows_the_upstream_pattern`.
      for (const id of ['kimi', '1st', 'MiniMax Token Plan', '阶跃', 'my-provider_1 x']) {
        expect(
          createProviderRequestSchema.safeParse({ ...createBody, id }).success,
          `${id} must be accepted`,
        ).toBe(true);
      }
      for (const id of [
        '',
        '   ',
        '-leading-dash',
        '_leading',
        'has:colon',
        'managed:kimi-code', // the engine's internal OAuth namespace
        'line\nbreak',
      ]) {
        expect(
          createProviderRequestSchema.safeParse({ ...createBody, id }).success,
          `${JSON.stringify(id)} must be rejected`,
        ).toBe(false);
      }
    });

    it('round-trips the replace body (with rename) and responses', () => {
      // Replace has no `id` (the path carries it); `new_id` renames.
      const { id: _id, ...createWithoutId } = createBody;
      const replaceBody = { ...createWithoutId, new_id: 'acme-2' };
      expect(replaceProviderRequestSchema.parse(replaceBody)).toEqual(replaceBody);
      expect(replaceProviderResponseSchema.parse({ provider })).toEqual({ provider });
      expect(deleteProviderResponseSchema.parse({ deleted: true })).toEqual({
        deleted: true,
      });
    });
  });

  describe('catalog browse and import', () => {
    const catalogProvider = {
      id: 'moonshot',
      name: 'Moonshot',
      wire_type: 'kimi',
      base_url: 'https://api.example.test/v1',
      guessed: false,
      needs_base_url: false,
      rejected: false,
      reject_reason: null,
      env_key: 'MOONSHOT_API_KEY',
      models: [{ id: 'k2', max_context_size: 131072, reasoning: true }],
    };

    it('round-trips catalog provider list and item', () => {
      expect(listCatalogProvidersResponseSchema.parse({ items: [catalogProvider] })).toEqual({
        items: [catalogProvider],
      });
      expect(getCatalogProviderResponseSchema.parse(catalogProvider)).toEqual(catalogProvider);
    });

    it('round-trips the import bodies and registry response', () => {
      expect(
        importCatalogProviderRequestSchema.parse({ catalog_id: 'moonshot', api_key: 'sk-x' }),
      ).toEqual({ catalog_id: 'moonshot', api_key: 'sk-x' });
      expect(
        importRegistryProviderRequestSchema.parse({ url: 'https://registry.example.test/api.json' }),
      ).toEqual({ url: 'https://registry.example.test/api.json' });
      expect(
        importCustomRegistryResponseSchema.parse({
          providers: [provider],
          models_imported: 3,
          credential_env: { acme: 'ACME_API_KEY' },
        }),
      ).toEqual({
        providers: [provider],
        models_imported: 3,
        credential_env: { acme: 'ACME_API_KEY' },
      });
    });
  });
});
