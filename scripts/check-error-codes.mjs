#!/usr/bin/env bun
/**
 * Error-code declaration/emission audit and ratchet.
 *
 * A TS declaration is not a Rust porting task by itself. The ratchet records
 * each current gap with an owner and fails on new drift. Resolved drift is
 * reported as a stale baseline entry for manual cleanup, not a failure.
 * Only production emitters count: Rust cfg(test) items, external test modules,
 * TS tests, generated output, and declaration/reason tables are excluded.
 */
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const ROOT = resolve(import.meta.dirname, '..');
const BASELINE_PATH = join(ROOT, 'scripts/error-code-baseline.json');
const TS_DECLARATIONS = 'packages/protocol/src/error-codes.ts';
const RUST_ENVELOPE = 'packages/kimi-agent/src/server/envelope.rs';
const CATEGORIES = new Set(['host-owned', 'rust-owned-unemitted', 'rust-undeclared']);

const read = (file) => readFileSync(join(ROOT, file), 'utf8');
const lineAt = (text, index) => text.slice(0, index).split('\n').length;
const evidence = (file, text, index) => `${file}:${lineAt(text, index)}`;

function listFiles() {
  const out = execFileSync('git', ['ls-files', '--cached', '--others', '--exclude-standard'], {
    cwd: ROOT,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  });
  return out.split('\n').filter(Boolean);
}

function parseTsDeclarations() {
  const text = read(TS_DECLARATIONS);
  const block = text.match(/export const ErrorCode = \{([\s\S]*?)\n\} as const;/)?.[1];
  if (!block) throw new Error(`Cannot locate ErrorCode declaration block in ${TS_DECLARATIONS}`);
  const blockOffset = text.indexOf(block);
  const declarations = new Map();
  for (const match of block.matchAll(/^\s*([A-Z][A-Z0-9_]*):\s*(\d+),/gm)) {
    const name = match[1];
    const code = Number(match[2]);
    if (declarations.has(name)) throw new Error(`Duplicate TS ErrorCode name: ${name}`);
    declarations.set(name, {
      name,
      code,
      evidence: evidence(TS_DECLARATIONS, text, blockOffset + match.index),
    });
  }
  return declarations;
}

function parseRustErrorCodeConstants() {
  const text = read(RUST_ENVELOPE);
  const block = text.match(/pub mod error_codes \{([\s\S]*?)\n\}/)?.[1];
  if (!block) throw new Error(`Cannot locate error_codes module in ${RUST_ENVELOPE}`);
  const blockOffset = text.indexOf(block);
  const constants = new Map();
  for (const match of block.matchAll(/pub const ([A-Z][A-Z0-9_]*): u32 = (\d+);/g)) {
    constants.set(match[1], {
      name: match[1],
      code: Number(match[2]),
      evidence: evidence(RUST_ENVELOPE, text, blockOffset + match.index),
    });
  }
  return constants;
}

/** End of the Rust item following a cfg(test) attribute. */
function testItemEnd(content, start) {
  let depth = 0;
  for (let i = start; i < content.length; i++) {
    const char = content[i];
    if (char === '/' && content[i + 1] === '/') {
      const newline = content.indexOf('\n', i + 2);
      if (newline === -1) return content.length;
      i = newline;
      continue;
    }
    if (char === '/' && content[i + 1] === '*') {
      let commentDepth = 1;
      i += 2;
      while (i < content.length && commentDepth > 0) {
        if (content.startsWith('/*', i)) {
          commentDepth++;
          i += 2;
        } else if (content.startsWith('*/', i)) {
          commentDepth--;
          i += 2;
        } else {
          i++;
        }
      }
      i--;
      continue;
    }
    const rawString =
      char === 'r' || char === 'b' || char === 'c'
        ? content.slice(i).match(/^(?:r|br|cr)(#*)"/)
        : null;
    if (rawString) {
      const terminator = `"${rawString[1]}`;
      const close = content.indexOf(terminator, i + rawString[0].length);
      if (close === -1) return content.length;
      i = close + terminator.length - 1;
      continue;
    }
    if (char === '"') {
      i++;
      while (i < content.length && content[i] !== '"') {
        i += content[i] === '\\' ? 2 : 1;
      }
      continue;
    }
    const charLiteral =
      char === "'"
        ? content
            .slice(i)
            .match(/^'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|[\s\S])|[^\\'])'/u)
        : null;
    if (charLiteral) {
      i += charLiteral[0].length - 1;
      continue;
    }
    if (char === '{') depth++;
    else if (char === '}') {
      depth--;
      if (depth === 0) return i + 1;
    } else if (char === ';' && depth === 0) return i + 1;
  }
  return content.length;
}

/** Mask cfg(test) items without moving production offsets or line numbers. */
function maskTestItems(content) {
  const attribute = /^[ \t]*#\[cfg\(test\)\]/gm;
  let masked = '';
  let copiedThrough = 0;
  let match;
  while ((match = attribute.exec(content)) !== null) {
    const start = match.index;
    const end = testItemEnd(content, start);
    masked += content.slice(copiedThrough, start);
    masked += content.slice(start, end).replaceAll(/[^\r\n]/g, ' ');
    copiedThrough = end;
    attribute.lastIndex = end;
  }
  return masked + content.slice(copiedThrough);
}

/** Mask comments while preserving strings, raw strings, offsets, and lines. */
function maskComments(content) {
  const chars = content.split('');
  let i = 0;
  while (i < content.length) {
    if (content.startsWith('//', i)) {
      while (i < content.length && content[i] !== '\n') chars[i++] = ' ';
      continue;
    }
    if (content.startsWith('/*', i)) {
      let depth = 1;
      chars[i++] = ' ';
      chars[i++] = ' ';
      while (i < content.length && depth > 0) {
        if (content.startsWith('/*', i)) {
          depth++;
          chars[i++] = ' ';
          chars[i++] = ' ';
        } else if (content.startsWith('*/', i)) {
          depth--;
          chars[i++] = ' ';
          chars[i++] = ' ';
        } else {
          if (content[i] !== '\n' && content[i] !== '\r') chars[i] = ' ';
          i++;
        }
      }
      continue;
    }
    const rawString =
      content[i] === 'r' || content[i] === 'b' || content[i] === 'c'
        ? content.slice(i).match(/^(?:r|br|cr)(#*)"/)
        : null;
    if (rawString) {
      const terminator = `"${rawString[1]}`;
      const close = content.indexOf(terminator, i + rawString[0].length);
      if (close === -1) break;
      i = close + terminator.length;
      continue;
    }
    if (content[i] === '"') {
      i++;
      while (i < content.length && content[i] !== '"') {
        i += content[i] === '\\' ? 2 : 1;
      }
      i++;
      continue;
    }
    const charLiteral =
      content[i] === "'"
        ? content
            .slice(i)
            .match(/^'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|[\s\S])|[^\\'])'/u)
        : null;
    if (charLiteral) {
      i += charLiteral[0].length;
      continue;
    }
    i++;
  }
  return chars.join('');
}

function addEmission(map, code, name, site) {
  if (!Number.isInteger(code) || code === 0) return;
  if (!map.has(code)) map.set(code, { code, name: name ?? null, evidence: [] });
  const entry = map.get(code);
  if (name && !entry.name) entry.name = name;
  if (!entry.evidence.includes(site)) entry.evidence.push(site);
}

function collectRustEmissions(files, errorCodeConstants) {
  const emissions = new Map();
  const constantDefinitions = [];
  const production = new Map();

  for (const file of files) {
    const original = read(file);
    const text = maskComments(maskTestItems(original));
    production.set(file, text);
    for (const match of text.matchAll(/\b(?:pub\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*u32\s*=\s*(\d+)\s*;/g)) {
      constantDefinitions.push({
        file,
        name: match[1],
        code: Number(match[2]),
        evidence: evidence(file, text, match.index),
      });
    }
  }

  const resolveCode = (file, token) => {
    if (/^\d+$/.test(token)) return { code: Number(token), name: null };
    const errorCodeName = token.match(/error_codes::([A-Z][A-Z0-9_]*)/)?.[1];
    if (errorCodeName) {
      const constant = errorCodeConstants.get(errorCodeName);
      return constant ? { code: constant.code, name: errorCodeName } : null;
    }
    const name = token.split('::').pop();
    const local = constantDefinitions.find((item) => item.file === file && item.name === name);
    const candidates = constantDefinitions.filter((item) => item.name === name);
    const constant = local ?? (candidates.length === 1 ? candidates[0] : null);
    return constant ? { code: constant.code, name: constant.name } : null;
  };

  for (const [file, text] of production) {
    // Constant references in this server module are response construction,
    // status-to-code mappings, or error tuples. Test and comment uses are gone.
    for (const match of text.matchAll(/\berror_codes::([A-Z][A-Z0-9_]*)\b/g)) {
      const name = match[1];
      const constant = errorCodeConstants.get(name);
      if (!constant) throw new Error(`Unknown Rust error_codes::${name} at ${evidence(file, text, match.index)}`);
      addEmission(emissions, constant.code, name, evidence(file, text, match.index));
    }

    // Explicit numeric or named codes carried directly by a JSON body.
    for (const match of text.matchAll(/["']code["']\s*:\s*([A-Za-z0-9_:]+|\d+)/g)) {
      const resolved = resolveCode(file, match[1]);
      if (resolved) addEmission(emissions, resolved.code, resolved.name, evidence(file, text, match.index));
    }

    // (callable, code argument position), counted from zero.
    const callPatterns = [
      [/\berr_envelope\s*\(/g, 0],
      [/\benvelope_err\s*\(/g, 1],
      [/\bws_protocol::ack\s*\(/g, 1],
      [/\berror_frame\s*\(/g, 0],
      [/\b(?:unauthorized_with_code|unauthorized_challenge)\s*\(/g, 0],
    ];
    for (const [pattern, position] of callPatterns) {
      for (const call of text.matchAll(pattern)) {
        const tail = text.slice(call.index + call[0].length);
        const args = tail.split(/,|\)/, position + 1);
        if (args.length <= position) continue;
        const token = args[position].trim();
        if (!/^[A-Za-z0-9_:]+$/.test(token)) continue;
        const resolved = resolveCode(file, token);
        if (resolved) addEmission(emissions, resolved.code, resolved.name, evidence(file, text, call.index));
      }
    }
  }
  return emissions;
}

function isTsProductionFile(file) {
  if (!/^(?:apps|packages)\//.test(file)) return false;
  if (!/\.(?:ts|tsx|mts|cts|js|jsx|mjs|cjs)$/.test(file) || file.endsWith('.d.ts')) return false;
  if (/(?:^|\/)(?:node_modules|dist|dist-web|dist-native|coverage|target|test|tests|__tests__)(?:\/|$)/.test(file)) return false;
  if (/\.(?:test|spec)\.[^.]+$/.test(file)) return false;
  return file !== TS_DECLARATIONS;
}

function collectHostEmissions(files, declarations) {
  const emissions = new Map();
  for (const file of files.filter(isTsProductionFile)) {
    const text = read(file);
    for (const match of text.matchAll(/\berrEnvelope\s*\(\s*(?:ErrorCode\.([A-Z][A-Z0-9_]*)|(\d+))/g)) {
      const name = match[1] ?? null;
      const declaration = name ? declarations.get(name) : null;
      const code = declaration?.code ?? (match[2] ? Number(match[2]) : null);
      if (code !== null) addEmission(emissions, code, name, evidence(file, text, match.index));
    }
  }
  return emissions;
}

function currentAttribution(declarations, rustConstants, rustEmissions, hostEmissions) {
  const entries = [];
  const declaredCodes = new Set();
  for (const declaration of declarations.values()) {
    if (declaration.code === 0) continue;
    declaredCodes.add(declaration.code);
    if (rustEmissions.has(declaration.code)) continue;
    const host = hostEmissions.get(declaration.code);
    const rustConstant = [...rustConstants.values()].find((item) => item.code === declaration.code);
    entries.push({
      category: host ? 'host-owned' : 'rust-owned-unemitted',
      code: declaration.code,
      name: declaration.name,
      rustState: host ? 'not-applicable' : rustConstant ? 'constant-only' : 'no-rust-constant',
      tsEvidence: declaration.evidence,
      rustConstantEvidence: rustConstant?.evidence ?? null,
      hostEvidence: host?.evidence ?? [],
      rustEvidence: [],
    });
  }
  for (const emission of rustEmissions.values()) {
    if (declaredCodes.has(emission.code)) continue;
    entries.push({
      category: 'rust-undeclared',
      code: emission.code,
      name: null,
      rustState: 'emitted',
      tsEvidence: null,
      rustConstantEvidence: null,
      hostEvidence: [],
      rustEvidence: emission.evidence,
    });
  }
  return entries.toSorted((a, b) => a.category.localeCompare(b.category) || a.code - b.code);
}

function signature(entry) {
  return JSON.stringify([entry.category, entry.code, entry.name, entry.rustState]);
}

function readBaseline() {
  const parsed = JSON.parse(readFileSync(BASELINE_PATH, 'utf8'));
  if (!Array.isArray(parsed.entries)) throw new Error('error-code-baseline.json must contain entries[]');
  const seen = new Set();
  for (const entry of parsed.entries) {
    if (!CATEGORIES.has(entry.category) || !Number.isInteger(entry.code) || typeof entry.rustState !== 'string') {
      throw new Error(`Malformed baseline entry: ${JSON.stringify(entry)}`);
    }
    if (typeof entry.attribution !== 'string' || entry.attribution.trim() === '') {
      throw new Error(`Baseline entry lacks attribution: ${entry.category}:${entry.code}`);
    }
    const key = `${entry.category}:${entry.code}`;
    if (seen.has(key)) throw new Error(`Duplicate baseline entry: ${key}`);
    seen.add(key);
  }
  return parsed.entries;
}

function printEntry(entry) {
  const sites = [
    entry.tsEvidence ? `TS ${entry.tsEvidence}` : null,
    entry.rustConstantEvidence ? `Rust const ${entry.rustConstantEvidence}` : null,
    entry.hostEvidence.length > 0 ? `host emit ${entry.hostEvidence.join(', ')}` : null,
    entry.rustEvidence.length > 0 ? `Rust emit ${entry.rustEvidence.join(', ')}` : null,
  ].filter(Boolean);
  console.log(`  - ${entry.code} ${entry.name ?? '(no TS name)'} [${entry.rustState}] ${sites.join('; ')}`);
}

let declarations;
let rustConstants;
try {
  declarations = parseTsDeclarations();
  rustConstants = parseRustErrorCodeConstants();
  if (declarations.size === 0 || rustConstants.size === 0) throw new Error('Empty declaration extractor result');
} catch (error) {
  console.error(`check-error-codes: extractor failed: ${error.message}`);
  process.exit(2);
}

const files = listFiles().filter((file) => existsSync(join(ROOT, file)));
const rustFiles = files.filter(
  (file) =>
    file.startsWith('packages/kimi-agent/src/server/') &&
    file.endsWith('.rs') &&
    !/(?:^|\/)(?:tests|tests\.rs)(?:\/|$)/.test(file.slice('packages/kimi-agent/src/server/'.length)),
);
const rustEmissions = collectRustEmissions(rustFiles, rustConstants);
const hostEmissions = collectHostEmissions(files, declarations);
const current = currentAttribution(declarations, rustConstants, rustEmissions, hostEmissions);
if (rustEmissions.size === 0) {
  console.error('check-error-codes: extractor found zero Rust emissions; refusing to trust a dark scan.');
  process.exit(2);
}

const declaredNonZero = [...declarations.values()].filter((item) => item.code !== 0);
const rustDeclaredNonZero = [...rustConstants.values()].filter((item) => item.code !== 0);
const emittedDeclared = declaredNonZero.filter((item) => rustEmissions.has(item.code));
console.log(
  `check-error-codes: TS declared=${declaredNonZero.length}; Rust constants=${rustDeclaredNonZero.length}; ` +
    `Rust-emitted declared=${emittedDeclared.length}; total drift=${current.length}`,
);
for (const category of CATEGORIES) {
  const entries = current.filter((entry) => entry.category === category);
  const detail =
    category === 'rust-owned-unemitted'
      ? ` (constant-only=${entries.filter((entry) => entry.rustState === 'constant-only').length}, no-rust-constant=${entries.filter((entry) => entry.rustState === 'no-rust-constant').length})`
      : '';
  console.log(`${category}: ${entries.length}${detail}`);
  for (const entry of entries) printEntry(entry);
}

const declarationMismatches = [];
for (const [name, declaration] of declarations) {
  const rust = rustConstants.get(name);
  if (rust && rust.code !== declaration.code) {
    declarationMismatches.push(`${name}: TS ${declaration.code} (${declaration.evidence}) != Rust ${rust.code} (${rust.evidence})`);
  }
}

let baseline;
try {
  baseline = readBaseline();
} catch (error) {
  console.error(`check-error-codes: baseline failed: ${error.message}`);
  process.exit(2);
}
const currentSignatures = new Map(current.map((entry) => [`${entry.category}:${entry.code}`, signature(entry)]));
const baselineSignatures = new Map(baseline.map((entry) => [`${entry.category}:${entry.code}`, signature(entry)]));
const added = [...currentSignatures].filter(([key, value]) => baselineSignatures.get(key) !== value);
const stale = [...baselineSignatures].filter(([key, value]) => currentSignatures.get(key) !== value);

for (const mismatch of declarationMismatches) console.error(`NEW DECLARATION DRIFT: ${mismatch}`);
for (const [key] of added) console.error(`NEW EMISSION DRIFT: ${String(key)} is absent or changed in the baseline`);
for (const [key] of stale) console.error(`STALE BASELINE: ${String(key)} is no longer present with its recorded attribution`);
console.log(
  `baseline: ${baseline.length} entries; new drift=${added.length + declarationMismatches.length}; stale=${stale.length} (report only)`,
);
if (added.length > 0 || declarationMismatches.length > 0) process.exit(1);
console.log('check-error-codes: OK; no error-code declaration/emission drift.');
