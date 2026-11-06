import { expect, test } from 'vitest';

import { checkConfigRegistry } from './check-config-registry.mjs';

const params = new Set(['native_llm', 'providers', 'shell_path']);

function registry(sections) {
  return { sections };
}

const ok = (extra = {}) => ({
  parsedBy: 'engine',
  rustConsumer: 'config/mod.rs',
  napiFields: [],
  ...extra,
});

test('a fully registered surface passes and reports the split', () => {
  const { failures, byMode, fieldCount } = checkConfigRegistry(
    registry({
      providers: ok({ parsedBy: 'both', napiFields: ['native_llm', 'providers'] }),
      shell: ok({ napiFields: ['shell_path'] }),
      raw: ok({ parsedBy: 'host', rustConsumer: '—' }),
    }),
    ['providers', 'shell', 'raw'],
    params,
  );
  expect(failures).toEqual([]);
  expect(byMode).toEqual({ engine: 1, host: 1, both: 1 });
  expect(fieldCount).toBe(3);
});

test('a schema key with no registry entry fails', () => {
  const { failures } = checkConfigRegistry(registry({ providers: ok() }), ['providers', 'mcp'], params);
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain('UNREGISTERED');
  expect(failures[0]).toContain('mcp');
});

test('a registry entry the schema dropped fails', () => {
  const { failures } = checkConfigRegistry(
    registry({ providers: ok(), retired: ok() }),
    ['providers'],
    params,
  );
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain('STALE');
  expect(failures[0]).toContain('retired');
});

test('a napi field that JsRunTurnParams dropped fails', () => {
  const { failures } = checkConfigRegistry(
    registry({ providers: ok({ napiFields: ['native_llm', 'long_gone_field'] }) }),
    ['providers'],
    params,
  );
  expect(failures).toHaveLength(1);
  expect(failures[0]).toContain('MISSING-NAPI');
  expect(failures[0]).toContain('long_gone_field');
});

test('a host-parsed section cannot ride the napi seam', () => {
  const { failures } = checkConfigRegistry(
    registry({ raw: ok({ parsedBy: 'host', rustConsumer: '—', napiFields: ['native_llm'] }) }),
    ['raw'],
    params,
  );
  expect(failures.some((f) => f.includes('CONTRADICTION'))).toBe(true);
});

test('an engine-parsed section with no consumer and no field is a contradiction', () => {
  const { failures } = checkConfigRegistry(
    registry({ mystery: ok({ rustConsumer: '—' }) }),
    ['mystery'],
    params,
  );
  expect(failures.some((f) => f.includes('CONTRADICTION'))).toBe(true);
});

test('engineOnly sections are exempt from the schema comparison, but not from parsedBy', () => {
  const pass = checkConfigRegistry(
    registry({ cron: ok({ engineOnly: true }) }),
    [],
    params,
  );
  expect(pass.failures).toEqual([]);

  const wrongMode = checkConfigRegistry(
    registry({ cron: ok({ engineOnly: true, parsedBy: 'both' }) }),
    [],
    params,
  );
  expect(wrongMode.failures.some((f) => f.includes('CONTRADICTION'))).toBe(true);

  // An engineOnly section that the schema *does* declare means the flag is
  // obsolete, not that the section is fine.
  const nowDeclared = checkConfigRegistry(
    registry({ cron: ok({ engineOnly: true }) }),
    ['cron'],
    params,
  );
  expect(nowDeclared.failures.some((f) => f.includes('STALE'))).toBe(true);
});

test('a bad parsedBy value is reported rather than silently counted', () => {
  const { failures } = checkConfigRegistry(
    registry({ providers: ok({ parsedBy: 'sometimes' }) }),
    ['providers'],
    params,
  );
  expect(failures.some((f) => f.includes('BAD-VALUE'))).toBe(true);
});
