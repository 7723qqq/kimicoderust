#!/usr/bin/env bun
/**
 * File-level migration gate for v2.
 *
 * Default mode checks v2 src/agent against scripts/agent-parity-manifest.json.
 * `--manifest scripts/v2-parity-manifest.json` switches to the v2-wide mode:
 * multiple roots, namespaced unit paths, per-unit baselines, and the additional
 * verdicts unported/planned/ui. The v2 manifest inherits the 288 agent rows
 * from the agent manifest instead of copying or rewriting their verdicts.
 *
 * The baseline ratchets red verdicts by path and count. Structural failures
 * always stay red. Update a baseline only with --update-baseline.
 * Set KIMI_V2_ROOT for the multi-unit mode, or KIMI_V2_AGENT_ROOT for the
 * default agent-only mode, when the reference checkout is not at the recorded
 * path.
 */

import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { isAbsolute, join, relative, resolve } from 'node:path';

const ROOT = resolve(import.meta.dirname, '..');
const MANIFEST_PATH = join(ROOT, 'scripts', 'agent-parity-manifest.json');
const BASELINE_PATH = join(ROOT, 'scripts', 'agent-parity-baseline.json');
const VERDICTS = ['equivalent', 'partial', 'different', 'na', 'missing'];
const VALID_VERDICTS = new Set(VERDICTS);
const RED_VERDICTS = new Set(['partial', 'different', 'missing']);
const V2_VERDICTS = ['equivalent', 'partial', 'different', 'missing', 'unported', 'planned', 'ui', 'na'];
const V2_VALID_VERDICTS = new Set(V2_VERDICTS);
const V2_RED_VERDICTS = new Set(['partial', 'different', 'missing', 'unported']);
const V2_VERIFIED_VERDICTS = new Set(['equivalent', 'partial', 'different', 'missing']);
const EXAMPLE_LIMIT = 20;
const UPDATE_BASELINE = process.argv.includes('--update-baseline');
const BASELINE_NOTE =
  'Red verdict baseline for check-agent-parity. The current red count must not exceed this ' +
  'baseline and every current red path must already be listed here. Other gate failures are ' +
  'never baselined. Update only with bun run check:agent-parity:update after review.';

const toPosix = (path) => path.replaceAll('\\', '/');
const compareItems = (a, b) => a.path.localeCompare(b.path);

function listV2Files(root) {
  const files = [];
  const pending = [''];
  while (pending.length) {
    const directory = pending.pop();
    for (const entry of readdirSync(join(root, directory), { withFileTypes: true })) {
      const relativePath = directory ? join(directory, entry.name) : entry.name;
      if (entry.isDirectory()) pending.push(relativePath);
      else if (entry.isFile() && entry.name.endsWith('.ts')) files.push(toPosix(relativePath));
    }
  }
  return files.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

function printExamples(title, items, format) {
  if (!items.length) return;
  console.log('\n' + title + ' (' + items.length + '):');
  for (const item of items.slice(0, EXAMPLE_LIMIT)) console.log('  ' + format(item));
  if (items.length > EXAMPLE_LIMIT) {
    console.log('  ... ' + (items.length - EXAMPLE_LIMIT) + ' more');
  }
}

function readBaseline() {
  if (!existsSync(BASELINE_PATH)) return null;
  const baseline = JSON.parse(readFileSync(BASELINE_PATH, 'utf8'));
  if (!Array.isArray(baseline.redEntries)) throw new Error('baseline.redEntries must be an array');
  const paths = new Set();
  for (const entry of baseline.redEntries) {
    if (typeof entry?.tsPath !== 'string' || !entry.tsPath || toPosix(entry.tsPath) !== entry.tsPath) {
      throw new Error('every baseline red entry needs a normalized tsPath');
    }
    if (!RED_VERDICTS.has(entry.verdict)) {
      throw new Error('baseline verdict must be partial/different/missing: ' + entry.tsPath);
    }
    if (paths.has(entry.tsPath)) throw new Error('duplicate baseline path: ' + entry.tsPath);
    paths.add(entry.tsPath);
  }
  return baseline;
}

function baselineContent(redEntries) {
  const rows = redEntries.map((entry) => `    ${JSON.stringify(entry)}`).join(',\n');
  return `{\n  "note": ${JSON.stringify(BASELINE_NOTE)},\n  "redEntries": [\n${rows}\n  ]\n}\n`;
}

function readArg(name) {
  const args = process.argv.slice(2);
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === name) {
      const value = args[index + 1];
      if (!value || value.startsWith('--')) throw new Error(name + ' requires a value');
      return value;
    }
    if (arg.startsWith(name + '=')) {
      const value = arg.slice(name.length + 1);
      if (!value) throw new Error(name + ' requires a value');
      return value;
    }
  }
  return null;
}

function globToRegExp(glob) {
  let out = '^';
  for (let index = 0; index < glob.length; index += 1) {
    const char = glob[index];
    if (char === '*') {
      if (glob[index + 1] === '*') {
        index += 1;
        if (glob[index + 1] === '/') {
          index += 1;
          out += '(?:.*/)?';
        } else {
          out += '.*';
        }
      } else {
        out += '[^/]*';
      }
    } else if (char === '?') {
      out += '[^/]';
    } else {
      out += char.replace(/[.+^${}()|[\]\\]/g, '\\$&');
    }
  }
  return new RegExp(out + '$');
}

function walkUnitFiles(root, out = []) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (['node_modules', 'target', 'dist', 'coverage', 'build', 'out'].includes(entry.name) || entry.name.startsWith('.')) continue;
    const abs = join(root, entry.name);
    if (entry.isDirectory()) walkUnitFiles(abs, out);
    else if (entry.isFile()) out.push(abs);
  }
  return out;
}

function listUnitFiles(root, defaults, unit) {
  const include = [...(defaults?.include || []), ...(unit.include || [])].map(globToRegExp);
  const exclude = [...(defaults?.exclude || []), ...(unit.exclude || [])].map(globToRegExp);
  if (!include.length) throw new Error('unit ' + unit.id + ' has no include patterns');
  return walkUnitFiles(root)
    .map((abs) => toPosix(relative(root, abs)))
    .filter((rel) => include.some((regex) => regex.test(rel)) && !exclude.some((regex) => regex.test(rel)))
    .sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

function readV2Baseline(unit) {
  const path = resolve(ROOT, unit.baseline);
  if (!existsSync(path)) return null;
  const baseline = JSON.parse(readFileSync(path, 'utf8'));
  if (!Array.isArray(baseline.redEntries)) throw new Error('baseline.redEntries must be an array: ' + unit.baseline);
  if (baseline.unit !== undefined && baseline.unit !== unit.id) {
    throw new Error('baseline unit mismatch: ' + unit.baseline);
  }
  const paths = new Set();
  for (const entry of baseline.redEntries) {
    if (typeof entry?.tsPath !== 'string' || !entry.tsPath || toPosix(entry.tsPath) !== entry.tsPath) {
      throw new Error('every baseline red entry needs a normalized tsPath: ' + unit.id);
    }
    if (!V2_RED_VERDICTS.has(entry.verdict)) {
      throw new Error('baseline verdict must be one of ' + [...V2_RED_VERDICTS].join('/') + ': ' + entry.tsPath);
    }
    if (paths.has(entry.tsPath)) throw new Error('duplicate baseline path: ' + entry.tsPath);
    paths.add(entry.tsPath);
  }
  return baseline;
}

function v2BaselineContent(unit, redEntries) {
  const date = new Intl.DateTimeFormat('en-CA', { timeZone: 'Asia/Shanghai' }).format(new Date());
  const note =
    'Initial unverified v2 parity baseline for ' + unit.id + '. Generated ' + date +
    ' by the machine pass. unported rows are not individually verified; candidateRustPaths are path/name/protocol ' +
    'candidates, not equivalence. This file is a ratchet snapshot, not proof of parity. Update only with ' +
    'bun run check:v2-parity:update after review.';
  return JSON.stringify({ note, kind: 'unverified-initial', unit: unit.id, redEntries }, null, 2) + '\n';
}

function loadV2Manifest(manifestPath) {
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  if (!Array.isArray(manifest.units) || !manifest.units.length) throw new Error('manifest.units must be a non-empty array');
  if (!Array.isArray(manifest.entries)) throw new Error('manifest.entries must be an array');
  if (!manifest.v2Root) throw new Error('manifest.v2Root is required');
  const ids = new Set();
  for (const unit of manifest.units) {
    if (typeof unit?.id !== 'string' || !unit.id) throw new Error('every unit needs an id');
    if (ids.has(unit.id)) throw new Error('duplicate unit id: ' + unit.id);
    ids.add(unit.id);
    if (typeof unit.root !== 'string' || !unit.root) throw new Error('unit ' + unit.id + ' needs a root');
    if (typeof unit.baseline !== 'string' || !unit.baseline) throw new Error('unit ' + unit.id + ' needs a baseline');
    if (unit.inherit && (typeof unit.inherit.manifest !== 'string' || typeof unit.inherit.prefix !== 'string')) {
      throw new Error('unit ' + unit.id + ' has an invalid inherit block');
    }
  }
  const v2Root = resolve(process.env.KIMI_V2_ROOT || manifest.v2Root);
  if (!existsSync(v2Root)) throw new Error('v2 root not found: ' + v2Root);

  const rows = [];
  for (const entry of manifest.entries) {
    if (typeof entry?.unit !== 'string' || !ids.has(entry.unit)) {
      throw new Error('manifest entry has an unknown unit: ' + JSON.stringify(entry?.tsPath ?? entry));
    }
    rows.push({ ...entry, inherited: false });
  }
  for (const unit of manifest.units) {
    if (!unit.inherit) continue;
    const sourcePath = resolve(ROOT, unit.inherit.manifest);
    if (!existsSync(sourcePath)) throw new Error('inherited manifest not found: ' + unit.inherit.manifest);
    const source = JSON.parse(readFileSync(sourcePath, 'utf8'));
    if (!Array.isArray(source.entries)) throw new Error('inherited manifest entries must be an array: ' + unit.inherit.manifest);
    const prefix = unit.inherit.prefix.endsWith('/') ? unit.inherit.prefix : unit.inherit.prefix + '/';
    if (source.v2Root && resolve(source.v2Root) !== resolve(v2Root, unit.root, prefix)) {
      throw new Error('inherited manifest v2Root does not match unit ' + unit.id);
    }
    for (const entry of source.entries) {
      rows.push({ ...entry, unit: unit.id, tsPath: prefix + toPosix(entry.tsPath), inherited: true });
    }
  }
  return { manifest, v2Root, rows };
}

function checkV2Unit(unit, rows, v2Root, defaults) {
  const root = resolve(v2Root, unit.root);
  if (!existsSync(root)) throw new Error('unit root not found: ' + root);
  const files = listUnitFiles(root, defaults, unit);
  const fileSet = new Set(files);
  const unitRows = rows.filter((entry) => entry.unit === unit.id);
  const seen = new Set();
  const counts = Object.fromEntries(V2_VERDICTS.map((verdict) => [verdict, 0]));
  const blocking = [];
  const uncovered = [];
  const zombies = [];
  const invalidRows = [];
  const duplicateRows = [];
  const emptyNaReasons = [];
  const emptyRustPaths = [];

  for (const [index, entry] of unitRows.entries()) {
    const path = typeof entry?.tsPath === 'string' ? toPosix(entry.tsPath) : '';
    const label = path || 'row ' + (index + 1);
    const invalid = [];
    if (typeof entry?.tsPath !== 'string' || !entry.tsPath) {
      invalid.push('tsPath is required');
    } else if (path !== entry.tsPath || path.startsWith('./') || path.split('/').includes('..') || isAbsolute(entry.tsPath)) {
      invalid.push('tsPath must be a normalized relative path with / separators');
    }
    if (!Array.isArray(entry?.rustPaths) || entry.rustPaths.some((item) => typeof item !== 'string')) {
      invalid.push('rustPaths must be a string array');
    }
    if (entry?.candidateRustPaths !== undefined &&
        (!Array.isArray(entry.candidateRustPaths) || entry.candidateRustPaths.some((item) => typeof item !== 'string' || !item.trim() || isAbsolute(item) || item.split('/').includes('..')))) {
      invalid.push('candidateRustPaths must be a repo-relative string array');
    }
    if (!Array.isArray(entry?.evidence) || entry.evidence.some((item) => typeof item !== 'string' || !item.trim())) {
      invalid.push('evidence must be a string array');
    }
    if (typeof entry?.note !== 'string') invalid.push('note must be a string');
    if (typeof entry?.naReason !== 'string') invalid.push('naReason must be a string');
    if (entry?.lines !== undefined && (!Number.isInteger(entry.lines) || entry.lines < 0)) invalid.push('lines must be a non-negative integer');
    if (!V2_VALID_VERDICTS.has(entry?.verdict)) {
      invalid.push('verdict must be one of ' + V2_VERDICTS.join('/'));
    } else {
      counts[entry.verdict] += 1;
      if (V2_RED_VERDICTS.has(entry.verdict)) blocking.push({ path, verdict: entry.verdict, note: entry.note });
    }
    if (entry?.verdict === 'na' && (typeof entry.naReason !== 'string' || !entry.naReason.trim())) emptyNaReasons.push({ path });
    if (entry?.verdict === 'na' && !entry.inherited) invalid.push('na is reserved for inherited agent rows; use unported');
    if (entry?.verdict === 'equivalent' && Array.isArray(entry.rustPaths) && !entry.rustPaths.length) emptyRustPaths.push({ path });
    if (V2_VERIFIED_VERDICTS.has(entry?.verdict) && Array.isArray(entry.evidence) && !entry.evidence.length) {
      invalid.push('verified verdicts need evidence');
    }
    if (entry?.verdict === 'unported' && Array.isArray(entry.rustPaths) && entry.rustPaths.length) {
      invalid.push('unported rows must not claim rustPaths; use candidateRustPaths');
    }
    if (entry?.verdict === 'ui' && !unit.ui) invalid.push('ui verdict is only allowed in a ui unit');
    if (unit.ui && entry?.verdict !== 'ui') invalid.push('ui units only accept ui rows');
    if (seen.has(path)) duplicateRows.push({ path, row: label });
    seen.add(path);
    if (invalid.length) invalidRows.push({ path: label, details: invalid });
  }

  for (const path of files) if (!seen.has(path)) uncovered.push({ path });
  for (const path of seen) if (!fileSet.has(path)) zombies.push({ path });
  const baseline = readV2Baseline(unit);
  if (!baseline && !UPDATE_BASELINE) {
    throw new Error('baseline not found for ' + unit.id + '; run bun run check:v2-parity:update');
  }
  const baselineRed = new Map((baseline?.redEntries ?? []).map((entry) => [entry.tsPath, entry.verdict]));
  const currentVerdictRedPaths = new Set(blocking.map((entry) => entry.path));
  const newRedEntries = blocking.filter((entry) => !baselineRed.has(entry.path));
  const resolvedRedEntries = [...baselineRed]
    .filter(([path]) => !currentVerdictRedPaths.has(path))
    .map(([path, verdict]) => ({ path, verdict }));
  const countExceeded = blocking.length > baselineRed.size;
  const currentRedPaths = new Set([...currentVerdictRedPaths, ...uncovered.map((item) => item.path), ...zombies.map((item) => item.path)]);
  for (const item of [...invalidRows, ...duplicateRows, ...emptyNaReasons, ...emptyRustPaths]) currentRedPaths.add(item.path);
  const hardFailureCount =
    uncovered.length + zombies.length + invalidRows.length + duplicateRows.length +
    emptyNaReasons.length + emptyRustPaths.length;
  blocking.sort(compareItems);
  uncovered.sort(compareItems);
  zombies.sort(compareItems);
  invalidRows.sort(compareItems);
  duplicateRows.sort(compareItems);
  return {
    unit,
    files,
    rows: unitRows.length,
    counts,
    blocking,
    baselineRed,
    newRedEntries,
    resolvedRedEntries,
    countExceeded,
    uncovered,
    zombies,
    invalidRows,
    duplicateRows,
    emptyNaReasons,
    emptyRustPaths,
    currentRedPaths,
    hardFailureCount,
  };
}

function runV2Gate(manifestPathArg) {
  const allowed = new Set(['--manifest', '--unit', '--update-baseline']);
  const args = process.argv.slice(2);
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === '--manifest' || arg === '--unit') {
      if (!args[index + 1] || args[index + 1].startsWith('--')) throw new Error(arg + ' requires a value');
      index += 1;
    } else if (arg.startsWith('--manifest=') || arg.startsWith('--unit=')) {
      continue;
    } else if (!allowed.has(arg)) {
      throw new Error('unknown argument: ' + arg);
    }
  }
  const manifestPath = resolve(ROOT, manifestPathArg);
  if (!existsSync(manifestPath)) throw new Error('manifest not found: ' + manifestPath);
  const unitFilter = readArg('--unit');
  const { manifest, v2Root, rows } = loadV2Manifest(manifestPath);
  const units = unitFilter ? manifest.units.filter((unit) => unit.id === unitFilter) : manifest.units;
  if (!units.length) throw new Error('unit not found: ' + unitFilter);

  const results = units.map((unit) => checkV2Unit(unit, rows, v2Root, manifest.defaults));
  const verdictTotals = Object.fromEntries(V2_VERDICTS.map((verdict) => [verdict, 0]));
  for (const result of results) for (const verdict of V2_VERDICTS) verdictTotals[verdict] += result.counts[verdict];
  const fileCount = results.reduce((sum, result) => sum + result.files.length, 0);
  const rowCount = results.reduce((sum, result) => sum + result.rows, 0);
  const selectedIds = new Set(units.map((unit) => unit.id));
  const localRowCount = rows.filter((entry) => selectedIds.has(entry.unit) && !entry.inherited).length;
  const inheritedRowCount = rows.filter((entry) => selectedIds.has(entry.unit) && entry.inherited).length;
  const currentRedCount = results.reduce((sum, result) => sum + result.blocking.length, 0);
  const baselineRedCount = results.reduce((sum, result) => sum + result.baselineRed.size, 0);
  const newRedCount = results.reduce((sum, result) => sum + result.newRedEntries.length, 0);
  const hardFailureCount = results.reduce((sum, result) => sum + result.hardFailureCount, 0);

  console.log('V2 parity gate');
  console.log('  v2 root: ' + v2Root);
  console.log('  manifest: ' + manifestPath);
  console.log('  pinned commit: ' + (manifest.pinnedCommit || 'not recorded'));
  console.log('  units: ' + units.length + ' / ' + manifest.units.length);
  console.log('  v2 TS files: ' + fileCount);
  console.log('  manifest rows: ' + rowCount + ' (local ' + localRowCount + ', inherited ' + inheritedRowCount + ')');
  console.log('  verdicts: ' + V2_VERDICTS.map((verdict) => verdict + '=' + verdictTotals[verdict]).join(' '));
  console.log('  red verdicts: current=' + currentRedCount + ' baseline=' + baselineRedCount + ' new=' + newRedCount);
  console.log('  structural failures: ' + hardFailureCount);
  for (const result of results) {
    console.log('  ' + result.unit.id.padEnd(34) +
      ' files=' + String(result.files.length).padStart(4) +
      ' rows=' + String(result.rows).padStart(4) +
      ' red=' + String(result.blocking.length).padStart(4) +
      ' new=' + String(result.newRedEntries.length).padStart(3) +
      ' structural=' + result.hardFailureCount);
  }

  if (UPDATE_BASELINE) {
    if (hardFailureCount) {
      console.log('\nBaseline update refused: fix structural failures first.');
      process.exitCode = 1;
      return;
    }
    for (const result of results) {
      const path = resolve(ROOT, result.unit.baseline);
      const redEntries = result.blocking.map((entry) => ({ tsPath: entry.path, verdict: entry.verdict }));
      writeFileSync(path, v2BaselineContent(result.unit, redEntries));
      console.log('  baseline updated: ' + result.unit.id + ' (' + redEntries.length + ' red entries)');
    }
    console.log('\nV2 parity baseline updated explicitly.');
    process.exitCode = 0;
    return;
  }

  for (const result of results) {
    printExamples('New red entries [' + result.unit.id + ']', result.newRedEntries, (item) =>
      '[' + item.verdict + '] ' + item.path + (item.note ? ' — ' + item.note : ''));
    printExamples('Uncovered files [' + result.unit.id + ']', result.uncovered, (item) => item.path);
    printExamples('Zombie rows [' + result.unit.id + ']', result.zombies, (item) => item.path);
    printExamples('Invalid rows [' + result.unit.id + ']', result.invalidRows, (item) => item.path + ': ' + item.details.join('; '));
    printExamples('Duplicate rows [' + result.unit.id + ']', result.duplicateRows, (item) => item.path);
    if (result.blocking.length < result.baselineRed.size) {
      console.log('  ' + result.unit.id + ': red count is below baseline; lower it explicitly with bun run check:v2-parity:update');
    }
  }

  const failed = hardFailureCount > 0 || newRedCount > 0 || results.some((result) => result.countExceeded);
  console.log(
    '\nV2 parity: ' + (failed ? 'RED' : 'GREEN') +
    ' (current=' + currentRedCount + ' baseline=' + baselineRedCount + ' new=' + newRedCount +
    ' structural=' + hardFailureCount + ')',
  );
  process.exitCode = failed ? 1 : 0;
}

function main() {
  const unknownArgs = process.argv.slice(2).filter((arg) => arg !== '--update-baseline');
  if (unknownArgs.length) throw new Error('unknown arguments: ' + unknownArgs.join(' '));

  const manifest = JSON.parse(readFileSync(MANIFEST_PATH, 'utf8'));
  if (!Array.isArray(manifest.entries)) throw new Error('manifest.entries must be an array');
  const configuredRoot = process.env.KIMI_V2_AGENT_ROOT || manifest.v2Root;
  if (!configuredRoot) throw new Error('manifest.v2Root or KIMI_V2_AGENT_ROOT is required');
  const v2Root = resolve(configuredRoot);
  if (!existsSync(v2Root)) throw new Error('v2 agent directory not found: ' + v2Root);

  const v2Files = listV2Files(v2Root);
  if (!v2Files.length) throw new Error('no TypeScript files found under: ' + v2Root);
  const v2Set = new Set(v2Files);
  const manifestSet = new Set();
  const verdictCounts = Object.fromEntries(VERDICTS.map((verdict) => [verdict, 0]));
  const blockingVerdicts = [];
  const emptyNaReasons = [];
  const emptyRustPaths = [];
  const duplicateRows = [];
  const invalidRows = [];
  const currentRedPaths = new Set();

  for (const [index, entry] of manifest.entries.entries()) {
    const rowLabel = 'row ' + (index + 1);
    const rawPath = typeof entry?.tsPath === 'string' ? entry.tsPath : rowLabel;
    const path = toPosix(rawPath);
    const invalid = [];

    if (typeof entry?.tsPath !== 'string' || !entry.tsPath) invalid.push('tsPath is required');
    if (path !== rawPath || path.startsWith('./') || path.split('/').includes('..')) {
      invalid.push('tsPath must be a normalized relative path with / separators');
    }
    if (isAbsolute(rawPath) || /^[A-Za-z]:/.test(rawPath)) invalid.push('tsPath must be relative');
    if (!Array.isArray(entry?.rustPaths) || entry.rustPaths.some((path) => typeof path !== 'string')) {
      invalid.push('rustPaths must be a string array');
    }
    if (!Array.isArray(entry?.evidence) || !entry.evidence.length ||
        entry.evidence.some((item) => typeof item !== 'string' || !item.trim())) {
      invalid.push('evidence must be a non-empty string array');
    }
    if (typeof entry?.note !== 'string') invalid.push('note must be a string');
    if (typeof entry?.naReason !== 'string') invalid.push('naReason must be a string');

    if (VALID_VERDICTS.has(entry?.verdict)) {
      verdictCounts[entry.verdict]++;
      if (RED_VERDICTS.has(entry.verdict)) {
        blockingVerdicts.push({ path, verdict: entry.verdict, note: entry.note });
        currentRedPaths.add(path);
      }
    } else {
      invalid.push('verdict must be one of ' + VERDICTS.join('/'));
    }

    if (entry?.verdict === 'na' && (typeof entry.naReason !== 'string' || !entry.naReason.trim())) {
      emptyNaReasons.push({ path });
      currentRedPaths.add(path);
    }
    if (entry?.verdict === 'equivalent' && Array.isArray(entry.rustPaths) && !entry.rustPaths.length) {
      emptyRustPaths.push({ path });
      currentRedPaths.add(path);
    }

    if (manifestSet.has(path)) {
      duplicateRows.push({ path, row: rowLabel });
      currentRedPaths.add(path);
    }
    manifestSet.add(path);

    if (invalid.length) {
      invalidRows.push({ path, details: invalid });
      currentRedPaths.add(path);
    }
  }

  const uncovered = v2Files.filter((path) => !manifestSet.has(path)).map((path) => ({ path }));
  const zombies = [...manifestSet].filter((path) => !v2Set.has(path)).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)).map((path) => ({ path }));
  for (const item of [...uncovered, ...zombies]) currentRedPaths.add(item.path);

  blockingVerdicts.sort(compareItems);
  emptyNaReasons.sort(compareItems);
  emptyRustPaths.sort(compareItems);
  duplicateRows.sort(compareItems);
  invalidRows.sort(compareItems);

  const hardFailureCount =
    uncovered.length + emptyNaReasons.length + emptyRustPaths.length +
    zombies.length + invalidRows.length + duplicateRows.length;
  const baseline = readBaseline();
  if (!baseline && !UPDATE_BASELINE) {
    throw new Error('baseline not found; create it explicitly with bun run check:agent-parity:update');
  }
  const baselineRed = new Map((baseline?.redEntries ?? []).map((entry) => [entry.tsPath, entry.verdict]));
  const currentVerdictRedPaths = new Set(blockingVerdicts.map((entry) => entry.path));
  const newRedEntries = blockingVerdicts.filter((entry) => !baselineRed.has(entry.path));
  const resolvedRedEntries = [...baselineRed]
    .filter(([path]) => !currentVerdictRedPaths.has(path))
    .map(([path, verdict]) => ({ path, verdict }));
  const countExceeded = blockingVerdicts.length > baselineRed.size;

  console.log('Agent parity gate');
  console.log('  v2 root: ' + v2Root);
  console.log('  pinned commit: ' + (manifest.pinnedCommit || 'not recorded'));
  console.log('  v2 TS files: ' + v2Files.length);
  console.log('  manifest rows: ' + manifest.entries.length + ' (' + manifestSet.size + ' unique paths)');
  console.log('  verdicts: ' + VERDICTS.map((verdict) => verdict + '=' + verdictCounts[verdict]).join(' '));
  console.log('  red verdicts: current=' + blockingVerdicts.length + ' baseline=' + baselineRed.size +
    ' new=' + newRedEntries.length + ' resolved=' + resolvedRedEntries.length);
  console.log('  all current red paths: ' + currentRedPaths.size);
  console.log(
    '  structural failures: uncovered=' + uncovered.length +
    ' emptyNaReasons=' + emptyNaReasons.length +
    ' emptyRustPaths=' + emptyRustPaths.length +
    ' zombies=' + zombies.length +
    ' invalidRows=' + invalidRows.length +
    ' duplicateRows=' + duplicateRows.length,
  );

  printExamples('New red entries', newRedEntries, (item) =>
    '[' + item.verdict + '] ' + item.path + (item.note ? ' — ' + item.note : ''));
  printExamples('Resolved baseline red entries', resolvedRedEntries, (item) =>
    '[' + item.verdict + '] ' + item.path);
  printExamples('v2 files missing from the manifest', uncovered, (item) => item.path);
  printExamples('NA rows without a reason', emptyNaReasons, (item) => item.path);
  printExamples('Equivalent rows without Rust paths', emptyRustPaths, (item) => item.path);
  printExamples('Zombie manifest rows', zombies, (item) => item.path);
  printExamples('Invalid manifest rows', invalidRows, (item) => item.path + ': ' + item.details.join('; '));
  printExamples('Duplicate manifest rows', duplicateRows, (item) => item.path + ' (' + item.row + ')');

  if (UPDATE_BASELINE) {
    if (hardFailureCount) {
      console.log('\nBaseline update refused: fix structural failures first.');
      process.exitCode = 1;
      return;
    }
    const redEntries = blockingVerdicts.map((entry) => ({ tsPath: entry.path, verdict: entry.verdict }));
    writeFileSync(BASELINE_PATH, baselineContent(redEntries));
    console.log('\nBaseline updated explicitly: ' + redEntries.length + ' red entries at ' + BASELINE_PATH);
    process.exitCode = 0;
    return;
  }

  if (blockingVerdicts.length < baselineRed.size) {
    console.log(
      '\nRed count is below the baseline. After review, run ' +
      'bun run check:agent-parity:update to lower it explicitly.',
    );
  }

  const failed = hardFailureCount > 0 || newRedEntries.length > 0 || countExceeded;
  console.log(
    '\nAgent parity: ' + (failed ? 'RED' : 'GREEN') +
    ' (current=' + blockingVerdicts.length + ' baseline=' + baselineRed.size +
    ' new=' + newRedEntries.length + ' countExceeded=' + countExceeded + ')',
  );
  process.exitCode = failed ? 1 : 0;
}

const V2_MANIFEST_ARG = readArg('--manifest');
if (V2_MANIFEST_ARG) {
  try {
    runV2Gate(V2_MANIFEST_ARG);
  } catch (error) {
    console.error('V2 parity gate failed: ' + (error instanceof Error ? error.message : String(error)));
    process.exitCode = 1;
  }
} else {
  try {
    main();
  } catch (error) {
    console.error('Agent parity gate failed: ' + (error instanceof Error ? error.message : String(error)));
    process.exitCode = 1;
  }
}
