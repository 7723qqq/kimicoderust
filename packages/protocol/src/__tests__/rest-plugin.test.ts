import { describe, expect, it } from 'vitest';

import {
  installPluginRequestSchema,
  installPluginResponseSchema,
  listPluginsResponseSchema,
  pluginMarketplaceResponseSchema,
  pluginRemoveResponseSchema,
  pluginSetEnabledResponseSchema,
} from '../index';

// Wire fixtures mirror the fork's `plugins.rs` serde shapes (camelCase).
describe('plugin REST schemas', () => {
  const summary = {
    id: 'demo',
    name: 'Demo Plugin',
    version: '1.2.3',
    enabled: true,
    description: 'A demo',
    source: 'github',
    originalSource: 'https://github.com/acme/demo',
    github: { owner: 'acme', repo: 'demo', ref: { kind: 'branch', value: 'HEAD' } },
    state: 'ok',
    skillCount: 2,
    mcpServerCount: 1,
    enabledMcpServerCount: 1,
    commandCount: 3,
    hookCount: 0,
    hasErrors: false,
  };

  it('round-trips the installed-plugin list', () => {
    expect(listPluginsResponseSchema.parse({ plugins: [summary] })).toEqual({
      plugins: [summary],
    });
  });

  it('accepts the fork-only remote state', () => {
    expect(
      listPluginsResponseSchema.parse({ plugins: [{ ...summary, state: 'remote' }] }),
    ).toEqual({ plugins: [{ ...summary, state: 'remote' }] });
  });

  it('round-trips a marketplace entry with its install record', () => {
    const entry = {
      id: 'demo',
      tier: 'official',
      displayName: 'Demo Plugin',
      description: 'A demo',
      homepage: 'https://example.test',
      keywords: ['demo'],
      source: './official/demo',
      version: '1.2.3',
      installed: { enabled: true, version: '1.2.3', installedAt: '2026-01-01T00:00:00Z' },
    };
    expect(pluginMarketplaceResponseSchema.parse({ entries: [entry] })).toEqual({
      entries: [entry],
    });
  });

  it('requires one of id / name / source on install', () => {
    expect(installPluginRequestSchema.parse({ source: './official/demo' })).toEqual({
      source: './official/demo',
    });
    expect(installPluginRequestSchema.safeParse({}).success).toBe(false);
  });

  it('round-trips the install and action responses', () => {
    expect(
      installPluginResponseSchema.parse({
        id: 'demo',
        enabled: true,
        version: '1.2.3',
        installed: true,
      }),
    ).toEqual({ id: 'demo', enabled: true, version: '1.2.3', installed: true });
    expect(
      pluginSetEnabledResponseSchema.parse({ ok: true, pluginId: 'demo', enabled: false }),
    ).toEqual({ ok: true, pluginId: 'demo', enabled: false });
    expect(
      pluginRemoveResponseSchema.parse({ ok: true, pluginId: 'demo', removed: true }),
    ).toEqual({ ok: true, pluginId: 'demo', removed: true });
  });
});
