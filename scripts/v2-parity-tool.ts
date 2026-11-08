#!/usr/bin/env bun
import { readdirSync, readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs';
import { join, extname, basename, relative } from 'node:path';

type Candidate = { rustPath: string; score: number; reasons: string[] };
type Row = { v2Path: string; v2Exports: string[]; best: Candidate | null; alternatives: Candidate[] };

const REPO = process.cwd();
const DEFAULT_V2_ROOT = 'D:/kimi/kimi-code/packages/agent-core-v2/src';
const RUST_ROOT = join(REPO, 'packages', 'kimi-agent', 'src');

const args = new Map<string, string>();
for (const a of Bun.argv.slice(2)) {
  const m = a.match(/^--([^=]+)(?:=(.*))?$/);
  if (m) args.set(m[1], m[2] ?? 'true');
}
const TOP = Number(args.get('top') ?? '3');
const MIN_SCORE = Number(args.get('min') ?? '0.12');
const OUT = args.get('out') ?? join('.tmp', 'v2-parity-map.json');
const V2_ROOT = args.get('v2-root') ?? DEFAULT_V2_ROOT;

function walk(root: string, exts: string[]): string[] {
  const out: string[] = [];
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop()!;
    let entries: any[];
    try { entries = readdirSync(dir, { withFileTypes: true }) as any; } catch { continue; }
    for (const e of entries) {
      const p = join(dir, e.name);
      if (e.isDirectory()) stack.push(p);
      else if (exts.includes(extname(e.name))) out.push(p);
    }
  }
  return out;
}

function readText(p: string): string {
  try { return readFileSync(p, 'utf8'); } catch { return ''; }
}

function tokens(s: string): Set<string> {
  const out = new Set<string>();
  for (const m of s.matchAll(/[A-Za-z_][A-Za-z0-9_]{2,}/g)) out.add(m[0]);
  return out;
}

function exportsOf(v2Text: string): string[] {
  const out = new Set<string>();
  for (const m of v2Text.matchAll(/export\s+(?:function|class|const|type|interface|enum|async function)\s+(\w+)/g))
    out.add(m[1]);
  return [...out];
}

function jaroish(a: string, b: string): number {
  if (!a || !b) return 0;
  a = a.toLowerCase(); b = b.toLowerCase();
  if (a === b) return 1;
  const maxDist = Math.max(Math.floor(Math.max(a.length, b.length) / 2) - 1, 0);
  const am = Array.from({ length: a.length }, () => false);
  const bm = Array.from({ length: b.length }, () => false);
  let matches = 0;
  for (let i = 0; i < a.length; i++) {
    const lo = Math.max(0, i - maxDist);
    const hi = Math.min(i + maxDist + 1, b.length);
    for (let j = lo; j < hi; j++) {
      if (bm[j] || a[i] !== b[j]) continue;
      am[i] = bm[j] = true;
      matches++;
      break;
    }
  }
  if (!matches) return 0;
  let t = 0, k = 0;
  for (let i = 0; i < a.length; i++) {
    if (!am[i]) continue;
    while (!bm[k]) k++;
    if (a[i] !== b[k]) t++;
    k++;
  }
  const jaro = (matches / a.length + matches / b.length + (matches - t / 2) / matches) / 3;
  let prefix = 0;
  for (let i = 0; i < Math.min(4, Math.min(a.length, b.length)); i++) {
    if (a[i] === b[i]) prefix++;
    else break;
  }
  return jaro + prefix * 0.1 * (1 - jaro);
}

function score(v2Path: string, v2Exports: string[], v2Tokens: Set<string>, rustPath: string, rustTokens: Set<string>) {
  const reasons: string[] = [];
  let s = 0;
  const vf = basename(v2Path, extname(v2Path)).replace(/[-]/g, '');
  const vSnake = vf.replace(/([a-z])([A-Z])/g, '$1_$2').toLowerCase();
  const rf = basename(rustPath, extname(rustPath)).replace(/[-_]/g, '');
  const rfSnake = basename(rustPath, extname(rustPath)).toLowerCase();
  const fnExact = vSnake === rfSnake || vf.toLowerCase() === basename(rustPath, extname(rustPath)).toLowerCase();
  const fnSim = fnExact ? 1 : jaroish(vf, rf);
  if (fnExact) { s += 0.7; reasons.push('filename:exact'); }
  else if (fnSim > 0.45) { s += fnSim * 0.35; reasons.push(`filename:${fnSim.toFixed(2)}`); }
  let symHit = 0;
  for (const e of v2Exports) if (rustTokens.has(e)) symHit++;
  if (v2Exports.length) {
    const symRatio = symHit / v2Exports.length;
    if (symRatio > 0) { s += symRatio * 0.35; reasons.push(`symbols:${symHit}/${v2Exports.length}`); }
  }
  let inter = 0;
  for (const t of v2Tokens) if (rustTokens.has(t)) inter++;
  const uni = v2Tokens.size + rustTokens.size - inter;
  const jacc = uni ? inter / uni : 0;
  if (jacc > 0.05) { s += jacc * 0.3; reasons.push(`identifiers:${(jacc * 100).toFixed(1)}%`); }
  return { score: s, reasons };
}

async function main() {
  if (!existsSync(V2_ROOT)) {
    console.error('v2 reference not found:', V2_ROOT);
    process.exit(2);
  }
  const v2Files = walk(V2_ROOT, ['.ts', '.tsx', '.js', '.mts']).sort((a, b) => a < b ? -1 : a > b ? 1 : 0);
  const rustFiles = walk(RUST_ROOT, ['.rs']).sort((a, b) => a < b ? -1 : a > b ? 1 : 0);
  console.log(`scanning ${v2Files.length} v2 files vs ${rustFiles.length} rust files...`);

  const rustData = rustFiles.map((p) => ({ path: p, tokens: tokens(readText(p)), rel: p.slice(REPO.length + 1) }));

  const rows: Row[] = [];
  let mapped = 0;
  for (const vf of v2Files) {
    const text = readText(vf);
    const vExports = exportsOf(text);
    const vTokens = tokens(text);
    const cands: Candidate[] = [];
    for (const rd of rustData) {
      const { score: sc, reasons } = score(vf, vExports, vTokens, rd.path, rd.tokens);
      if (sc >= MIN_SCORE) cands.push({ rustPath: rd.rel, score: sc, reasons });
    }
    cands.sort((a, b) => b.score - a.score);
    const best = cands[0] ?? null;
    if (best) mapped++;
    rows.push({ v2Path: relative(V2_ROOT, vf), v2Exports: vExports, best, alternatives: cands.slice(1, TOP) });
  }

  const byV2Dir = new Map<string, { files: number; mapped: number }>();
  for (const r of rows) {
    const dir = r.v2Path.split(/[\\/]/)[0] ?? r.v2Path;
    const cur = byV2Dir.get(dir) ?? { files: 0, mapped: 0 };
    cur.files++;
    if (r.best) cur.mapped++;
    byV2Dir.set(dir, cur);
  }

  const output = {
    generatedAt: new Date().toISOString(),
    v2Root: V2_ROOT,
    rustRoot: RUST_ROOT,
    totalV2: rows.length,
    mapped,
    unmapped: rows.length - mapped,
    dirCoverage: Object.fromEntries([...byV2Dir.entries()].sort((a, b) => a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)),
    rows,
  };

  if (!existsSync(join(REPO, '.tmp'))) mkdirSync(join(REPO, '.tmp'), { recursive: true });
  writeFileSync(join(REPO, OUT), JSON.stringify(output, null, 2));

  console.log(`\nresult: ${mapped}/${rows.length} v2 files have a candidate Rust counterpart (min score ${MIN_SCORE})`);
  console.log(`written: ${OUT}`);
  console.log('\nper-directory coverage (mapped/files):');
  for (const [d, c] of [...byV2Dir.entries()].sort((a, b) => a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)) {
    console.log(`  ${d.padEnd(45)} ${c.mapped}/${c.files}`);
  }
}

await main();
