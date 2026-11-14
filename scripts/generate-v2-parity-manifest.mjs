#!/usr/bin/env bun
/**
 * Builds the v2-wide file parity manifest.
 *
 * Only agent-core-v2/src/agent keeps its human-reviewed judgments: those rows
 * are inherited from scripts/agent-parity-manifest.json by the gate. Every row
 * this script emits is an initial, unverified machine row. Candidate Rust paths
 * are path/name/protocol matches only; they are not equivalence claims.
 */

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { parse } from '@babel/parser';

const ROOT = resolve(import.meta.dirname, '..');
const V2_ROOT = resolve(process.env.KIMI_V2_ROOT || 'D:/kimi/kimi-code');
const MANIFEST_PATH = join(ROOT, 'scripts', 'v2-parity-manifest.json');
const SUMMARY_JSON = join(ROOT, 'reports', 'v2-parity', 'v2-parity-initial-summary.json');
const SUMMARY_MD = join(ROOT, 'reports', 'v2-parity', 'v2-parity-initial-summary.md');
const BASELINE_DIR = 'scripts/v2-parity-baseline';

const DEFAULT_INCLUDE = ['**/*.ts', '**/*.tsx'];
const DEFAULT_EXCLUDE = [
  '**/*.test.ts', '**/*.test.tsx', '**/*.spec.ts', '**/*.spec.tsx',
  '**/*.d.ts', '**/*.d.mts', '**/*.d.cts',
  '**/test/**', '**/tests/**', '**/__tests__/**',
  '**/node_modules/**', '**/dist/**', '**/coverage/**', '**/build/**', '**/out/**',
];

const UNITS = [
  { id: 'packages/acp-server', package: 'acp-server', root: 'packages/acp-server/src' },
  {
    id: 'packages/agent-core-v2',
    package: 'agent-core-v2',
    root: 'packages/agent-core-v2/src',
    inherit: { manifest: 'scripts/agent-parity-manifest.json', prefix: 'agent/' },
  },
  { id: 'packages/i18n', package: 'i18n', root: 'packages/i18n/src' },
  { id: 'packages/i18n-shared', package: 'i18n-shared', root: 'packages/i18n-shared/src' },
  { id: 'packages/kap-server', package: 'kap-server', root: 'packages/kap-server/src' },
  { id: 'packages/klient', package: 'klient', root: 'packages/klient/src' },
  { id: 'packages/kosong', package: 'kosong', root: 'packages/kosong/src' },
  { id: 'packages/migration-legacy', package: 'migration-legacy', root: 'packages/migration-legacy/src' },
  { id: 'packages/minidb', package: 'minidb', root: 'packages/minidb/src' },
  { id: 'packages/node-sdk', package: 'node-sdk', root: 'packages/node-sdk/src' },
  { id: 'packages/oauth', package: 'oauth', root: 'packages/oauth/src' },
  { id: 'packages/pi-tui', package: 'pi-tui', root: 'packages/pi-tui/src', ui: true },
  { id: 'packages/product', package: 'product', root: 'packages/product/src' },
  { id: 'packages/remote-control', package: 'remote-control', root: 'packages/remote-control/src' },
  { id: 'packages/runtime-shared', package: 'runtime-shared', root: 'packages/runtime-shared/src' },
  { id: 'packages/session-import', package: 'session-import', root: 'packages/session-import/src' },
  { id: 'packages/telemetry', package: 'telemetry', root: 'packages/telemetry/src' },
  { id: 'packages/transcript', package: 'transcript', root: 'packages/transcript/src' },
  { id: 'packages/tree-sitter-bash', package: 'tree-sitter-bash', root: 'packages/tree-sitter-bash/src' },
  { id: 'apps/vis/web', package: 'apps/vis/web', root: 'apps/vis/web/src', ui: true },
  { id: 'apps/vis/server', package: 'apps/vis/server', root: 'apps/vis/server/src' },
  { id: 'apps/vscode/webview-ui', package: 'apps/vscode/webview-ui', root: 'apps/vscode/webview-ui/src', ui: true },
  { id: 'apps/vscode/src', package: 'apps/vscode', root: 'apps/vscode/src' },
  { id: 'apps/vscode/shared', package: 'apps/vscode', root: 'apps/vscode/shared' },
  { id: 'apps/kimi-code/src/tui', package: 'apps/kimi-code', root: 'apps/kimi-code/src/tui', ui: true },
  { id: 'apps/kimi-code/src', package: 'apps/kimi-code', root: 'apps/kimi-code/src', exclude: ['tui/**'] },
];

const PATH_GENERIC = new Set(['src', 'agent', 'service', 'services', 'impl', 'index', 'types', 'type', 'utils', 'util', 'common', 'core', 'lib', 'mod', 'main', 'internal', 'base']);
const STEM_GENERIC = new Set(['index', 'types', 'type', 'utils', 'util', 'constants', 'const', 'config', 'errors', 'error', 'events', 'event', 'client', 'server', 'service', 'services', 'main', 'mod', 'api', 'contract', 'schema', 'schemas', 'state', 'store', 'provider', 'providers', 'model', 'models', 'protocol', 'protocols', 'tools', 'tool', 'core', 'common', 'helpers', 'helper', 'setup', 'options', 'params', 'result', 'results', 'request', 'response', 'message', 'messages', 'tests', 'test']);
const SYMBOL_GENERIC = new Set(['error', 'config', 'client', 'server', 'service', 'state', 'store', 'event', 'message', 'request', 'response', 'result', 'options', 'params', 'context', 'provider', 'tool', 'model', 'session', 'agent', 'id', 'name', 'value', 'data', 'info', 'status', 'kind', 'type', 'entry', 'record', 'item', 'list', 'map', 'set', 'path', 'file', 'dir', 'command', 'task', 'goal', 'turn', 'run', 'new', 'default', 'from', 'into', 'get', 'set', 'start', 'stop', 'open', 'close', 'read', 'write', 'load', 'save', 'create', 'delete', 'update', 'build', 'parse', 'format', 'send', 'recv', 'wait', 'sync', 'init', 'main', 'args', 'input', 'output', 'handler', 'manager', 'factory', 'registry', 'collection', 'scope', 'token', 'usage', 'media', 'blob', 'checkpoint', 'prompt', 'profile', 'undo', 'schema', 'protocol', 'methods', 'events']);

const toPosix = (path) => path.replaceAll('\\', '/');
const splitWords = (value) => value
  .replace(/([a-z0-9])([A-Z])/g, '$1 $2')
  .replace(/([A-Z]+)([A-Z][a-z])/g, '$1 $2')
  .split(/[^A-Za-z0-9]+/)
  .filter(Boolean)
  .map((part) => part.toLowerCase());
const norm = (value) => splitWords(value).join('');
const significant = (tokens) => tokens.filter((token) => !PATH_GENERIC.has(token));

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

function walkFiles(root, out = []) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    if (entry.name === 'node_modules' || entry.name === 'target' || entry.name === 'dist' || entry.name === 'coverage' || entry.name === 'build' || entry.name === 'out' || entry.name.startsWith('.')) continue;
    const abs = join(root, entry.name);
    if (entry.isDirectory()) walkFiles(abs, out);
    else if (entry.isFile()) out.push(abs);
  }
  return out;
}

function listUnitFiles(unit) {
  const root = join(V2_ROOT, unit.root);
  if (!existsSync(root)) throw new Error('unit root not found: ' + root);
  const include = [...(unit.include || []), ...DEFAULT_INCLUDE].map(globToRegExp);
  const exclude = [...(unit.exclude || []), ...DEFAULT_EXCLUDE].map(globToRegExp);
  return walkFiles(root)
    .map((abs) => toPosix(relative(root, abs)))
    .filter((rel) => include.some((regex) => regex.test(rel)) && !exclude.some((regex) => regex.test(rel)))
    .sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

function lineCount(text) {
  const lines = text.split(/\r?\n/);
  if (lines.length && lines[lines.length - 1] === '') lines.pop();
  return lines.length;
}

function parseTsIndex(text) {
  const plugins = [
    ['typescript', 'jsx', 'decorators-legacy', 'classProperties', 'importAttributes', 'explicitResourceManagement', 'topLevelAwait'],
    ['typescript', 'jsx', 'decorators-legacy', 'importAttributes', 'topLevelAwait'],
    ['typescript', 'jsx'],
  ];
  let ast = null;
  let error = null;
  for (const pluginSet of plugins) {
    try {
      ast = parse(text, { sourceType: 'module', allowReturnOutsideFunction: true, errorRecovery: false, plugins: pluginSet });
      break;
    } catch (caught) {
      error = String(caught?.message || caught);
    }
  }
  if (!ast) return { exports: new Set(), strings: new Set(), error };
  const exports = new Set();
  const strings = new Set();
  const visit = (node) => {
    if (!node || typeof node !== 'object') return;
    if (Array.isArray(node)) {
      for (const item of node) visit(item);
      return;
    }
    if (typeof node.type !== 'string') return;
    if (node.type === 'ExportNamedDeclaration') {
      if (node.declaration) {
        const decl = node.declaration;
        if (decl.id?.name) exports.add(decl.id.name);
        if (decl.type === 'VariableDeclaration') {
          for (const item of decl.declarations) if (item.id?.name) exports.add(item.id.name);
        }
      }
      for (const specifier of node.specifiers || []) {
        const name = specifier.exported?.name || specifier.exported?.value || specifier.local?.name;
        if (name) exports.add(name);
      }
    }
    if (node.type === 'ExportDefaultDeclaration') {
      exports.add(node.declaration?.id?.name || 'default');
    }
    if (node.type === 'StringLiteral') {
      const value = node.value;
      if (value.length >= 5 && value.length <= 80 && /^[a-z][a-z0-9_.:-]*[a-z0-9]$/.test(value) && /[._:-]/.test(value)) strings.add(value);
    }
    for (const key of Object.keys(node)) {
      if (key === 'loc' || key === 'start' || key === 'end' || key.endsWith('Comments') || key === 'extra') continue;
      const value = node[key];
      if (value && typeof value === 'object') visit(value);
    }
  };
  visit(ast);
  return { exports, strings, error: null };
}

function longestCommonRun(a, b) {
  let best = 0;
  for (let i = 0; i < a.length; i += 1) {
    for (let j = 0; j < b.length; j += 1) {
      let run = 0;
      while (i + run < a.length && j + run < b.length && a[i + run] === b[j + run]) run += 1;
      if (run > best) best = run;
    }
  }
  return best;
}

function buildRustIndex() {
  const rustFiles = [];
  const cargoRoots = [];
  for (const parent of ['packages', 'apps']) {
    const base = join(ROOT, parent);
    if (!existsSync(base)) continue;
    for (const entry of readdirSync(base, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const dir = join(base, entry.name);
      if (existsSync(join(dir, 'Cargo.toml'))) cargoRoots.push(dir);
    }
  }
  for (const crateRoot of cargoRoots) {
    const cargo = readFileSync(join(crateRoot, 'Cargo.toml'), 'utf8');
    const crate = /^name\s*=\s*"([^"]+)"/m.exec(cargo)?.[1] || crateRoot.split(sep).at(-1);
    const srcRoot = join(crateRoot, 'src');
    if (!existsSync(srcRoot)) continue;
    for (const abs of walkFiles(srcRoot)) {
      if (!abs.endsWith('.rs')) continue;
      const rel = toPosix(relative(srcRoot, abs));
      if (rel.startsWith('tests/') || rel.includes('/tests/') || rel.endsWith('_test.rs') || /(^|\/)test_[^/]*\.rs$/.test(rel) || /(^|\/)tests\.rs$/.test(rel) || /(^|\/)test_support\.rs$/.test(rel)) continue;
      const parts = rel.replace(/\.rs$/, '').split('/');
      if (parts.at(-1) === 'mod') parts.pop();
      const text = readFileSync(abs, 'utf8');
      const items = new Set();
      for (const match of text.matchAll(/^\s*pub\s+(?:async\s+)?(?:unsafe\s+)?(?:fn|struct|enum|trait|type|const|static|mod)\s+([A-Za-z_][A-Za-z0-9_]*)/gm)) items.add(match[1]);
      const strings = new Set();
      for (const match of text.matchAll(/"([^"\\\n]{4,80})"/g)) {
        const value = match[1];
        if (/^[a-z][a-z0-9_.:-]*[a-z0-9]$/.test(value) && /[._:-]/.test(value)) strings.add(value);
      }
      rustFiles.push({
        crate,
        rustPath: toPosix(relative(ROOT, abs)),
        moduleTokens: parts.flatMap(splitWords),
        items,
        strings,
      });
    }
  }
  const itemMap = new Map();
  const stringMap = new Map();
  const tokenMap = new Map();
  const stemMap = new Map();
  for (const rustFile of rustFiles) {
    for (const item of rustFile.items) {
      const key = norm(item);
      if (!itemMap.has(key)) itemMap.set(key, new Set());
      itemMap.get(key).add(rustFile);
    }
    for (const value of rustFile.strings) {
      if (!stringMap.has(value)) stringMap.set(value, new Set());
      stringMap.get(value).add(rustFile);
    }
    for (const token of new Set(significant(rustFile.moduleTokens))) {
      if (!tokenMap.has(token)) tokenMap.set(token, new Set());
      tokenMap.get(token).add(rustFile);
    }
    const stem = rustFile.moduleTokens.at(-1);
    if (stem) {
      if (!stemMap.has(stem)) stemMap.set(stem, new Set());
      stemMap.get(stem).add(rustFile);
    }
  }
  return { rustFiles, itemMap, stringMap, tokenMap, stemMap };
}

function candidatesFor(tsPath, text, rustIndex) {
  const parts = toPosix(tsPath).replace(/\.(tsx?|mts|cts)$/, '').split('/');
  const stem = parts.pop();
  const stemTokens = splitWords(stem);
  const pathTokens = parts.flatMap(splitWords).concat(stemTokens);
  const sigPath = significant(pathTokens);
  const { exports, strings } = parseTsIndex(text);
  const candidates = new Set();
  for (const token of new Set(sigPath)) {
    for (const rustFile of rustIndex.tokenMap.get(token) || []) candidates.add(rustFile);
  }
  if (!STEM_GENERIC.has(stem.toLowerCase())) {
    for (const rustFile of rustIndex.stemMap.get(norm(stem)) || []) candidates.add(rustFile);
  }
  for (const name of exports) {
    const key = norm(name);
    if (key.length < 7 || SYMBOL_GENERIC.has(key)) continue;
    for (const rustFile of rustIndex.itemMap.get(key) || []) candidates.add(rustFile);
  }
  for (const value of strings) {
    for (const rustFile of rustIndex.stringMap.get(value) || []) candidates.add(rustFile);
  }

  const scored = [];
  const stemNorm = norm(stem);
  for (const rustFile of candidates) {
    const sigRust = significant(rustFile.moduleTokens);
    const rustStem = norm(rustFile.moduleTokens.at(-1) || '');
    let score = 0;
    const reasons = [];
    let exactStem = false;
    let pathRun = 0;
    let symbolHits = 0;
    if (stemNorm === rustStem && !STEM_GENERIC.has(stem.toLowerCase())) {
      score += 6;
      reasons.push('stem');
      exactStem = true;
    } else if (stemNorm.length >= 4 && rustStem.length >= 4 && !STEM_GENERIC.has(stem.toLowerCase()) && (stemNorm.startsWith(rustStem) || rustStem.startsWith(stemNorm))) {
      score += 2;
      reasons.push('stem-prefix');
    }
    const run = Math.max(longestCommonRun(pathTokens, rustFile.moduleTokens), longestCommonRun(sigPath, sigRust));
    pathRun = run;
    if (run >= 2) {
      score += run * 2;
      reasons.push('path:' + run);
    }
    const sigRustSet = new Set(sigRust);
    const overlap = [...new Set(sigPath)].filter((token) => sigRustSet.has(token)).length;
    if (overlap >= 2) {
      score += Math.min(overlap, 3);
      reasons.push('tokens:' + overlap);
    }
    for (const name of exports) {
      const key = norm(name);
      if (key.length < 7 || SYMBOL_GENERIC.has(key)) continue;
      const posting = rustIndex.itemMap.get(key);
      if (!posting || posting.size > 3 || !posting.has(rustFile)) continue;
      symbolHits += 1;
      if (symbolHits <= 2) {
        score += 4;
        reasons.push('symbol:' + name);
      }
    }
    let stringHits = 0;
    for (const value of strings) {
      const posting = rustIndex.stringMap.get(value);
      if (!posting || posting.size > 3 || !posting.has(rustFile)) continue;
      stringHits += 1;
      if (stringHits <= 2) {
        score += 3;
        reasons.push('protocol:' + value);
      }
    }
    const keep = (exactStem && score >= 6) || (pathRun >= 3 && score >= 6) || (symbolHits > 0 && score >= 6) || score >= 8;
    if (keep) scored.push({ rustFile, score, reasons: reasons.slice(0, 6) });
  }
  scored.sort((a, b) => b.score - a.score || a.rustFile.rustPath.localeCompare(b.rustFile.rustPath));
  return scored.slice(0, 3).map((item) => ({
    path: item.rustFile.rustPath,
    score: item.score,
    reasons: item.reasons,
  }));
}

function readAgentRows() {
  const sourcePath = join(ROOT, 'scripts', 'agent-parity-manifest.json');
  const source = JSON.parse(readFileSync(sourcePath, 'utf8'));
  if (!Array.isArray(source.entries)) throw new Error('agent parity manifest entries must be an array');
  const unit = UNITS.find((item) => item.id === 'packages/agent-core-v2');
  if (!unit) throw new Error('agent unit is not configured');
  if (source.v2Root && resolve(source.v2Root) !== join(V2_ROOT, unit.root, unit.inherit.prefix.replace(/\/$/, ''))) {
    throw new Error('agent parity v2Root does not match the configured unit root');
  }
  const agentRoot = join(V2_ROOT, unit.root);
  return source.entries.map((entry) => {
    const tsPath = unit.inherit.prefix + toPosix(entry.tsPath);
    const text = readFileSync(join(agentRoot, tsPath), 'utf8');
    return { ...entry, unit: unit.id, tsPath, lines: lineCount(text) };
  });
}

function readPreviousEntries() {
  const previous = new Map();
  if (!existsSync(MANIFEST_PATH)) return previous;
  const data = JSON.parse(readFileSync(MANIFEST_PATH, 'utf8'));
  if (!Array.isArray(data.entries)) return previous;
  for (const entry of data.entries) {
    if (typeof entry?.unit === 'string' && typeof entry?.tsPath === 'string') {
      previous.set(entry.unit + '\u0000' + entry.tsPath, entry);
    }
  }
  return previous;
}

function main() {
  if (!existsSync(V2_ROOT)) throw new Error('v2 root not found: ' + V2_ROOT);
  const rustIndex = buildRustIndex();
  const previousEntries = readPreviousEntries();
  const entries = [];
  const generatedLines = new Map();

  for (const unit of UNITS) {
    const files = listUnitFiles(unit);
    for (const tsPath of files) {
      const abs = join(V2_ROOT, unit.root, tsPath);
      if (!existsSync(abs)) continue;
      const text = readFileSync(abs, 'utf8');
      const lines = lineCount(text);
      generatedLines.set(unit.id + '/' + tsPath, lines);
      if (unit.inherit && tsPath.startsWith(unit.inherit.prefix)) continue;
      const tsIndex = parseTsIndex(text);
      const previous = previousEntries.get(unit.id + '\u0000' + tsPath);
      const reviewed = previous && (previous.reviewed === true || previous.verdict !== 'unported' || (Array.isArray(previous.evidence) && previous.evidence.length > 0));
      if (reviewed) {
        entries.push({ ...previous, unit: unit.id, tsPath, lines, exports: tsIndex.exports.size });
        continue;
      }
      if (unit.ui) {
        entries.push({
          unit: unit.id,
          tsPath,
          verdict: 'ui',
          rustPaths: [],
          candidateRustPaths: [],
          evidence: [],
          note: '前端/UI，迁移形态待定',
          naReason: '',
          lines,
          exports: tsIndex.exports.size,
          match: { score: 0, reasons: [] },
        });
        continue;
      }
      const candidates = candidatesFor(tsPath, text, rustIndex);
      entries.push({
        unit: unit.id,
        tsPath,
        verdict: 'unported',
        rustPaths: [],
        candidateRustPaths: candidates.map((item) => item.path),
        evidence: [],
        note: candidates.length
          ? '候选落点，待核；匹配仅基于路径/符号/协议名，未验证行为等价'
          : '未找到 Rust 候选落点；待核',
        naReason: '',
        lines,
        exports: tsIndex.exports.size,
        match: candidates.length
          ? { score: candidates[0].score, reasons: candidates[0].reasons }
          : { score: 0, reasons: [] },
      });
    }
  }

  const agentRows = readAgentRows();
  const totalRows = entries.length + agentRows.length;
  const totalFiles = UNITS.reduce((sum, unit) => sum + listUnitFiles(unit).length, 0);
  if (totalRows !== totalFiles) {
    throw new Error('manifest rows (' + totalRows + ') do not match unit files (' + totalFiles + ')');
  }

  let pinnedCommit = null;
  try {
    pinnedCommit = execFileSync('git', ['-C', V2_ROOT, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {
    pinnedCommit = null;
  }

  const manifest = {
    note: 'v2-wide file-level parity ledger. Scope: all v2 src .ts/.tsx (tests and declarations excluded). agent-core-v2/src/agent rows are inherited verbatim from scripts/agent-parity-manifest.json and keep their human-reviewed verdicts. Every other row is an initial, unverified machine row: unported means no verified Rust counterpart; candidateRustPaths are path/name/protocol matches only, not equivalence; evidence is intentionally empty. Baselines are unverified snapshots, not proof of parity. Regenerate machine rows with bun run generate:v2-parity; rows with reviewed:true, a non-unported verdict, or non-empty evidence are preserved; do not hand-edit unreviewed rows.',
    v2Root: toPosix(V2_ROOT),
    pinnedCommit,
    generatedAt: new Date().toISOString(),
    rowCount: totalRows,
    localRowCount: entries.length,
    inheritedRowCount: agentRows.length,
    defaults: { include: DEFAULT_INCLUDE, exclude: DEFAULT_EXCLUDE },
    units: UNITS.map((unit) => ({
      id: unit.id,
      package: unit.package,
      root: unit.root,
      ui: !!unit.ui,
      baseline: BASELINE_DIR + '/' + unit.id.replaceAll('/', '__') + '.json',
      ...(unit.inherit ? { inherit: unit.inherit } : {}),
      ...(unit.exclude ? { exclude: unit.exclude } : {}),
    })),
    entries,
  };
  mkdirSync(join(ROOT, 'scripts', 'v2-parity-baseline'), { recursive: true });
  writeFileSync(MANIFEST_PATH, JSON.stringify(manifest, null, 2) + '\n');

  const summary = buildSummary(manifest, agentRows, generatedLines);
  mkdirSync(join(ROOT, 'reports', 'v2-parity'), { recursive: true });
  writeFileSync(SUMMARY_JSON, JSON.stringify(summary, null, 2) + '\n');
  writeFileSync(SUMMARY_MD, summaryMarkdown(summary) + '\n');

  console.log('wrote ' + toPosix(relative(ROOT, MANIFEST_PATH)));
  console.log('units: ' + UNITS.length + ', src files: ' + totalFiles + ', new rows: ' + entries.length + ', inherited agent rows: ' + agentRows.length);
  console.log('candidate rows: ' + entries.filter((entry) => entry.candidateRustPaths.length).length);
  console.log('wrote ' + toPosix(relative(ROOT, SUMMARY_MD)));
}

function buildSummary(manifest, agentRows, generatedLines) {
  const unitById = new Map(manifest.units.map((unit) => [unit.id, unit]));
  const rows = [
    ...agentRows,
    ...manifest.entries.map((entry) => ({ ...entry, lines: generatedLines.get(entry.unit + '/' + entry.tsPath) || entry.lines || 0 })),
  ];
  const packages = new Map();
  for (const row of rows) {
    const unit = unitById.get(row.unit);
    if (!unit) throw new Error('unknown unit in summary: ' + row.unit);
    const pkg = unit.package;
    if (!packages.has(pkg)) packages.set(pkg, { package: pkg, files: 0, lines: 0, verdicts: {}, candidates: 0 });
    const target = packages.get(pkg);
    target.files += 1;
    target.lines += row.lines || 0;
    target.verdicts[row.verdict] ||= { files: 0, lines: 0 };
    target.verdicts[row.verdict].files += 1;
    target.verdicts[row.verdict].lines += row.lines || 0;
    if (row.candidateRustPaths?.length) target.candidates += 1;
  }
  const packageRows = [...packages.values()].sort((a, b) => b.lines - a.lines || a.package.localeCompare(b.package));
  const verdicts = ['equivalent', 'partial', 'different', 'missing', 'unported', 'planned', 'ui', 'na'];
  const totals = { files: rows.length, lines: rows.reduce((sum, row) => sum + (row.lines || 0), 0), verdicts: {}, candidates: 0 };
  for (const verdict of verdicts) totals.verdicts[verdict] = { files: 0, lines: 0 };
  for (const row of rows) {
    totals.verdicts[row.verdict] ||= { files: 0, lines: 0 };
    totals.verdicts[row.verdict].files += 1;
    totals.verdicts[row.verdict].lines += row.lines || 0;
    if (row.candidateRustPaths?.length) totals.candidates += 1;
  }
  const suspected = rows
    .filter((row) => row.verdict === 'unported' && !(row.unit === 'packages/agent-core-v2' && row.tsPath.startsWith('agent/')))
    .sort((a, b) => (b.lines || 0) - (a.lines || 0) || a.tsPath.localeCompare(b.tsPath))
    .slice(0, 30)
    .map((row) => ({
      package: unitById.get(row.unit).package,
      unit: row.unit,
      tsPath: row.tsPath,
      lines: row.lines || 0,
      candidateRustPaths: row.candidateRustPaths || [],
      note: row.note,
    }));
  return {
    generatedAt: manifest.generatedAt,
    v2Root: manifest.v2Root,
    pinnedCommit: manifest.pinnedCommit,
    scope: { files: totals.files, lines: totals.lines, testFilesExcluded: true, declarationsExcluded: true },
    note: 'Initial machine pass. Only agent-core-v2/src/agent rows carry human-reviewed verdicts; all other rows are unverified. candidateRustPaths are candidates, not equivalence.',
    packageRows,
    totals,
    suspectedMissingTop30: suspected,
  };
}

function summaryMarkdown(summary) {
  const verdicts = ['equivalent', 'partial', 'different', 'missing', 'unported', 'planned', 'ui', 'na'];
  const cell = (stats) => stats ? stats.files.toLocaleString('en-US') + ' / ' + stats.lines.toLocaleString('en-US') : '0 / 0';
  const lines = [];
  lines.push('# v2 parity 初始清单汇总');
  lines.push('');
  lines.push('- 数据时点（UTC）：' + summary.generatedAt);
  lines.push('- v2：`' + summary.v2Root + '`，commit `' + (summary.pinnedCommit || 'unknown') + '`');
  lines.push('- 范围：v2 全部 src `.ts`/`.tsx`，排除 test 与 `.d.ts`；agent 288 条沿用既有判定，其余为机器初判');
  lines.push('- 重要：`unported` 只表示“没有经过验证的 Rust 对应实现”，`candidateRustPaths` 只是路径/符号/协议名候选，**不是等价**；本 baseline 是未经核对的初值');
  lines.push('');
  lines.push('## 口径选择');
  lines.push('');
  lines.push('选择 **全部 src 2,569 文件 / 472,912 行**，而不是只做引擎 1,903 文件：');
  lines.push('');
  lines.push('- 前端 452 文件 / 115,405 行属于迁移范围，只是形态待定；用 `ui` 单列后仍应进入总账，否则会隐藏 24.4% 的 src 行数。');
  lines.push('- apps 非 UI 的 214 文件 / 42,737 行（CLI、VS Code 宿主、vis server）同样从未判定过，不能留在账外。');
  lines.push('- 测试 1,073 文件 / 432,085 行不进入文件级清单：Rust 测试与 TS 测试不是一一对应，逐文件记账会产生虚假覆盖率；测试等价另用行为用例清单。');
  lines.push('- `agent-core-v2/src/agent` 的 288 条不复制、不改写，由清单的 `inherit` 指向现有 `scripts/agent-parity-manifest.json`，判定值保持原样。');
  lines.push('');
  lines.push('## 汇总');
  lines.push('');
  lines.push('| 分类 | 文件 | 物理行 |');
  lines.push('|---|---:|---:|');
  for (const verdict of verdicts) {
    const stats = summary.totals.verdicts[verdict];
    if (stats) lines.push('| ' + verdict + ' | ' + stats.files.toLocaleString('en-US') + ' | ' + stats.lines.toLocaleString('en-US') + ' |');
  }
  lines.push('| **合计** | **' + summary.totals.files.toLocaleString('en-US') + '** | **' + summary.totals.lines.toLocaleString('en-US') + '** |');
  const partial = summary.totals.verdicts.partial || { files: 0, lines: 0 };
  const unported = summary.totals.verdicts.unported || { files: 0, lines: 0 };
  const equivalent = summary.totals.verdicts.equivalent || { files: 0, lines: 0 };
  lines.push('');
  lines.push('已验证等价：' + equivalent.files + ' 文件 / ' + equivalent.lines.toLocaleString('en-US') + ' 行；未完成（partial + unported）：' + (partial.files + unported.files) + ' 文件 / ' + (partial.lines + unported.lines).toLocaleString('en-US') + ' 行；UI 待定形态：' + (summary.totals.verdicts.ui?.files || 0) + ' 文件 / ' + (summary.totals.verdicts.ui?.lines || 0).toLocaleString('en-US') + ' 行。');
  lines.push('');
  lines.push('机器初判：' + summary.totals.files + ' 行中有 ' + summary.totals.candidates + ' 行带候选落点；1,829 条 unported 中 ' + summary.totals.candidates + ' 条有候选（' + ((summary.totals.candidates * 100 / (unported.files || 1)).toFixed(1)) + '%），' + (unported.files - summary.totals.candidates) + ' 条没有候选。候选全部未核，不能当作等价。`planned` 为 0：当前没有机器可读的排期清单，不把“好像有计划”当作 planned。');
  lines.push('');
  lines.push('## 按包');
  lines.push('');
  lines.push('| 包/单元 | 文件 | 行 | equivalent | partial | unported | planned | ui | na | 候选数 |');
  lines.push('|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|');
  for (const row of summary.packageRows) {
    lines.push('| ' + row.package + ' | ' + row.files + ' | ' + row.lines.toLocaleString('en-US') + ' | ' + cell(row.verdicts.equivalent) + ' | ' + cell(row.verdicts.partial) + ' | ' + cell(row.verdicts.unported) + ' | ' + cell(row.verdicts.planned) + ' | ' + cell(row.verdicts.ui) + ' | ' + cell(row.verdicts.na) + ' | ' + row.candidates + ' |');
  }
  lines.push('| **合计** | **' + summary.totals.files + '** | **' + summary.totals.lines.toLocaleString('en-US') + '** | **' + cell(summary.totals.verdicts.equivalent) + '** | **' + cell(summary.totals.verdicts.partial) + '** | **' + cell(summary.totals.verdicts.unported) + '** | **' + cell(summary.totals.verdicts.planned) + '** | **' + cell(summary.totals.verdicts.ui) + '** | **' + cell(summary.totals.verdicts.na) + '** | **' + summary.totals.candidates + '** |');
  lines.push('');
  lines.push('## 除 agent 288 条外疑似缺失 Top 30（按行数）');
  lines.push('');
  lines.push('排除 `agent-core-v2/src/agent` 的既有 288 条；同一包的其余 1,061 条仍计入。候选落点未核，不能当作等价。');
  lines.push('');
  lines.push('| # | 行 | 包 | 文件 | 候选落点（未核） |');
  lines.push('|---:|---:|---|---|---|');
  summary.suspectedMissingTop30.forEach((row, index) => {
    const candidate = row.candidateRustPaths.length ? row.candidateRustPaths[0] + (row.candidateRustPaths.length > 1 ? ' (+' + (row.candidateRustPaths.length - 1) + ')' : '') : '—';
    lines.push('| ' + (index + 1) + ' | ' + row.lines.toLocaleString('en-US') + ' | ' + row.package + ' | ' + row.tsPath + ' | ' + candidate + ' |');
  });
  lines.push('');
  lines.push('## 口径与限制');
  lines.push('');
  lines.push('- 覆盖 19 个 packages 的 `src`，以及 `apps/vis/web`、`apps/vis/server`、`apps/vscode/webview-ui`、`apps/vscode/src`、`apps/vscode/shared`、`apps/kimi-code/src`、`apps/kimi-code/src/tui`。');
  lines.push('- 测试 1,073 文件 / 432,085 行未进入文件级清单：Rust 测试与 TS 测试不是一一对应，逐文件台账会产生虚假覆盖率。');
  lines.push('- 前端 UI 统一为 `ui`，迁移单位应是屏幕/交互流程/协议，不是文件；`pi-tui` 按 UI 基础设施处理。');
  lines.push('- 机器匹配只索引 Rust `pub` 符号、模块路径和协议字符串；匹配到只写 `candidateRustPaths`，`rustPaths` 保持空，`evidence` 保持空，等待人工核对。');
  lines.push('- 候选阈值：文件 stem 精确匹配、路径连续 ≥3 段、或命中 Rust `pub` 符号且总分 ≥6，或总分 ≥8；每个 TS 文件最多列 3 个候选。');
  lines.push('- 候选不是“确认缺失”的反面：没有候选也不能证明 Rust 侧一定没有实现，只表示当前机器规则对不上；因此全部保持 `unported`。');
  lines.push('- baseline 是当前状态的快照，不代表已验收；任何新文件未登记都会红灯。');
  lines.push('');
  lines.push('## 产物与命令');
  lines.push('');
  lines.push('- 清单：`scripts/v2-parity-manifest.json`（26 个包/单元，2,281 条机器行 + 288 条继承的 agent 行）');
  lines.push('- 基线：`scripts/v2-parity-baseline/<unit>.json`，每包/单元一份，`kind` 为 `unverified-initial`');
  lines.push('- 重新生成机器行：`bun run generate:v2-parity`（`reviewed:true`、非 `unported` 判定或非空 `evidence` 的行会保留，不会被覆盖）');
  lines.push('- 检查：`bun run check:v2-parity`');
  lines.push('- 显式更新基线：`bun run check:v2-parity:update`');
  lines.push('- 旧 agent 门禁仍保留：`bun run check:agent-parity` / `bun run check:agent-parity:update`；v2 门禁通过 `inherit` 读取同一份 288 条判定，不复制、不改写。');
  return lines.join('\n');
}

try {
  main();
} catch (error) {
  console.error('v2 parity manifest generation failed: ' + (error instanceof Error ? error.message : String(error)));
  process.exitCode = 1;
}
