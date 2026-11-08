#!/usr/bin/env bun
/**
 * Config-surface registry gate (原 RUST-PORT-GAP §5 阶段 3 A8 方案 a；该台账已迁至
 * `reports/runbooks/v2-parity-ledger.md`).
 *
 * `packages/node-sdk/src/config-local/schema.ts` and
 * `packages/kimi-agent/src/config/mod.rs` each parse the same `config.toml`
 * sections. `scan-parity.mjs` already answers "does the key exist on both
 * sides"; what it cannot answer is "who actually interprets this section, and
 * what does it need to cross the napi seam to get there". That is the question
 * that decides how many layers one new config surface forces you to touch, and
 * it was being carried in people's heads.
 *
 * `scripts/config-registry.json` records it per section:
 *
 *   parsedBy      `engine` | `host` | `both`
 *   rustConsumer  the module that reads it (or `—` for host-only)
 *   napiFields    the `JsRunTurnParams` fields it rides the seam on
 *
 * The gate is two-way, like the other ratchets: a TS section with no registry
 * entry fails, and a registry entry for a section the schema no longer declares
 * fails too. A `napiFields` name that no longer exists on `JsRunTurnParams`
 * fails as well — a registry that names a field the boundary dropped is
 * describing a mechanism that is gone.
 *
 * Not checked on purpose: the Rust-only sections (`workspace`, `cron`,
 * `mcp_servers`, the snake_case aliases). Those are the engine's own surface and
 * already covered by scan-parity's Rust-key comparison; registering them here
 * would be a second list to keep in step with the first.
 */

import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const ROOT = resolve(import.meta.dirname, '..');
const REGISTRY = join(ROOT, 'scripts/config-registry.json');

const read = (p) => readFileSync(p, 'utf8');

function tsSectionNames() {
  const text = read(join(ROOT, 'packages/node-sdk/src/config-local/schema.ts'));
  const block = text.match(/export const KimiConfigSchema = z\.object\(\{([\s\S]*?)\n\}\);/)?.[1] ?? '';
  return [...new Set([...block.matchAll(/^\s{2}([A-Za-z_][A-Za-z0-9_]*):/gm)].map((m) => m[1]))].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

function napiParamFields() {
  const text = read(join(ROOT, 'packages/kimi-agent/src/napi_bindings/params.rs'));
  const struct = text.match(/pub struct JsRunTurnParams \{([\s\S]*?)\n\}/)?.[1] ?? '';
  return new Set([...struct.matchAll(/^\s{4}pub\s+([a-z_0-9]+)\s*:/gm)].map((m) => m[1]));
}

const PARSE_MODES = new Set(['engine', 'host', 'both']);

/**
 * Check a registry against the two live surfaces. Exported so the gate has its
 * own tests: a red gate proves nothing if the comparator itself is broken.
 */
export function checkConfigRegistry(registry, declared, params) {
  const sections = registry.sections ?? {};
  const registered = Object.keys(sections).sort();

  const failures = [];

  for (const name of declared) {
    if (!(name in sections)) {
      failures.push(`UNREGISTERED  schema.ts declares \`${name}\` but config-registry.json has no entry`);
    }
  }

  for (const name of registered) {
    const entry = sections[name];
    // `engineOnly` sections are the engine's own surface: the TS schema never
    // declared them, so "missing from schema.ts" is the expected state rather
    // than a stale entry. They stay registered so the split is visible.
    if (entry.engineOnly === true) {
      if (declared.includes(name)) {
        failures.push(
          `STALE  \`${name}\` is marked engineOnly but schema.ts now declares it — drop the flag`,
        );
      }
      if (entry.parsedBy !== 'engine') {
        failures.push(`CONTRADICTION  \`${name}\` is engineOnly but parsedBy = ${entry.parsedBy}`);
      }
      continue;
    }
    if (!declared.includes(name)) {
      failures.push(`STALE  config-registry.json records \`${name}\` but schema.ts no longer declares it`);
    }
    if (!PARSE_MODES.has(entry.parsedBy)) {
      failures.push(
        `BAD-VALUE  \`${name}\`.parsedBy = ${JSON.stringify(entry.parsedBy)} (want engine | host | both)`,
      );
    }
    if (typeof entry.rustConsumer !== 'string' || entry.rustConsumer.length === 0) {
      failures.push(`BAD-VALUE  \`${name}\`.rustConsumer must be a non-empty string (use "—" for none)`);
    }
    if (!Array.isArray(entry.napiFields)) {
      failures.push(`BAD-VALUE  \`${name}\`.napiFields must be an array`);
      continue;
    }
    for (const field of entry.napiFields) {
      if (!params.has(field)) {
        failures.push(
          `MISSING-NAPI  \`${name}\`.napiFields names \`${field}\`, which JsRunTurnParams no longer declares`,
        );
      }
    }
    if (entry.parsedBy === 'host' && entry.napiFields.length > 0) {
      failures.push(
        `CONTRADICTION  \`${name}\` is host-parsed but rides the napi seam on ${entry.napiFields.join(', ')}`,
      );
    }
    if (entry.parsedBy === 'engine' && entry.napiFields.length === 0 && entry.rustConsumer === '—') {
      failures.push(
        `CONTRADICTION  \`${name}\` is engine-parsed but names neither a rustConsumer nor a napi field`,
      );
    }
  }

  const byMode = { engine: 0, host: 0, both: 0 };
  let fieldCount = 0;
  for (const name of registered) {
    byMode[sections[name].parsedBy] += 1;
    fieldCount += sections[name].napiFields.length;
  }

  return { failures, registered, byMode, fieldCount };
}

function main() {
  const registry = JSON.parse(read(REGISTRY));
  const declared = tsSectionNames();
  const params = napiParamFields();
  const { failures, registered, byMode, fieldCount } = checkConfigRegistry(
    registry,
    declared,
    params,
  );

  if (failures.length > 0) {
    console.error(`check-config-registry: ${failures.length} problem(s)\n`);
    for (const f of failures) console.error(`  - ${f}`);
    console.error('\nThe registry is the list of what one config surface costs:');
    console.error('  scripts/config-registry.json');
    process.exit(1);
  }

  console.log(
    `check-config-registry: OK — ${registered.length} section(s) registered ` +
      `(engine=${byMode.engine} | host=${byMode.host} | both=${byMode.both}), ` +
      `${fieldCount} napi field reference(s) resolve against JsRunTurnParams (${params.size} fields).`,
  );
  console.log(`  schema: ${declared.length} top-level key(s), all registered.`);
}

if (import.meta.main ?? process.argv[1]?.endsWith('check-config-registry.mjs')) {
  main();
}
