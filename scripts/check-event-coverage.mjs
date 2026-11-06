#!/usr/bin/env bun
/**
 * Event coverage gate: every event the engine emits is classified for the UI.
 *
 * `EngineEvent` (packages/kimi-agent/src/events/types.rs) is the engine's own
 * vocabulary. `kimi-ui-core` classifies each name as either an entry or
 * transport-only (`PROJECTED_EVENTS` / `TRANSPORT_ONLY_EVENTS`). Two layers are
 * checked:
 *
 * 1. **No gaps.** An engine event in neither list is unaccounted — and an
 *    unaccounted event falls through a projector's `_ => Vec::new()` arm, which
 *    looks exactly like an event that genuinely has nothing to render. That is
 *    how `tool.call.completed`'s result went missing for months: the event was
 *    handled (it sealed the entry) while its payload was silently dropped.
 *
 * 2. **No fiction.** A name in either list that the engine no longer emits is a
 *    classification describing something that does not exist. It would keep
 *    passing forever because the gate's only other direction proves coverage,
 *    not relevance.
 *
 * Not checked: whether the projection *implements* the entry class fully. That
 * is a behaviour question with its own tests; this gate answers "did anyone
 * decide", which is the question that goes unanswered silently.
 */

import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const ROOT = resolve(import.meta.dirname, '..');
const EVENT_TYPES = join(ROOT, 'packages/kimi-agent/src/events/types.rs');
const CLASSIFICATION = join(ROOT, 'packages/kimi-ui-core/src/event_class.rs');

const read = (p) => readFileSync(p, 'utf8');

/** Wire names the engine can emit, from the serde renames on `EngineEvent`. */
function engineEventNames() {
  const text = read(EVENT_TYPES);
  const block = text.match(/pub enum EngineEvent \{([\s\S]*?)\n\}/)?.[1] ?? '';
  /** @type {Set<string>} */
  const names = new Set();
  for (const m of block.matchAll(/#\[serde\(rename = "([a-z_.]+)"\)\]/g)) names.add(m[1]);
  return [...names].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

/** One `const NAME: &[&str] = &[ "a", "b" ];` list from the classification file. */
function stringList(text, constName) {
  const pattern = new RegExp(`const ${constName}: &\\[&str\\] = &\\[([\\s\\S]*?)\\n\\];`);
  const block = text.match(pattern)?.[1];
  if (block === undefined) return null;
  return [...block.matchAll(/"([a-z_.]+)"/g)].map((m) => m[1]).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

function main() {
  const engine = engineEventNames();
  const text = read(CLASSIFICATION);
  const projected = stringList(text, 'PROJECTED_EVENTS');
  const transport = stringList(text, 'TRANSPORT_ONLY_EVENTS');

  const failures = [];
  if (projected === null) failures.push('PROJECTED_EVENTS is not declared in event_class.rs');
  if (transport === null) failures.push('TRANSPORT_ONLY_EVENTS is not declared in event_class.rs');
  if (failures.length > 0) {
    for (const f of failures) console.error(`  - ${f}`);
    process.exit(1);
  }

  const classified = new Set([...projected, ...transport]);

  for (const name of engine) {
    if (!classified.has(name)) {
      failures.push(
        `UNCLASSIFIED  engine emits \`${name}\` but neither PROJECTED_EVENTS nor TRANSPORT_ONLY_EVENTS names it`,
      );
    }
  }
  for (const name of classified) {
    if (!engine.includes(name)) {
      failures.push(
        `FICTION  classification names \`${name}\` but EngineEvent no longer emits it`,
      );
    }
  }
  for (const name of projected) {
    if (transport.includes(name)) {
      failures.push(`OVERLAP  \`${name}\` is in both lists`);
    }
  }

  if (failures.length > 0) {
    console.error(`check-event-coverage: ${failures.length} problem(s)\n`);
    for (const f of failures) console.error(`  - ${f}`);
    console.error('\nDecide what the UI does with each event:');
    console.error('  packages/kimi-ui-core/src/event_class.rs');
    process.exit(1);
  }

  console.log(
    `check-event-coverage: OK — ${engine.length} engine event(s): ` +
      `${projected.length} projected to entries, ${transport.length} transport-only.`,
  );
}

main();
