#!/usr/bin/env bun

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, extname, join, relative, resolve } from 'node:path';

const ROOT = resolve(import.meta.dirname, '..');
const PACKAGES = join(ROOT, 'packages');
const LIMIT = 0.0005;
const SOURCE_EXTENSIONS = new Set(['.rs', '.ts']);
const SKIPPED_DIRECTORIES = new Set([
  '.git',
  'node_modules',
  'target',
  'dist',
  'dist-web',
  'coverage',
]);
const TEST_DIRECTORIES = new Set(['test', 'tests', '__tests__']);
const GENERATED_FILES = new Set([
  'packages/kimi-agent/index.d.ts',
  'packages/kimi-agent/napi-contract.d.ts',
]);
const REGEX_PREFIX_KEYWORDS = new Set([
  'await',
  'case',
  'delete',
  'else',
  'in',
  'instanceof',
  'of',
  'return',
  'throw',
  'typeof',
  'void',
  'yield',
]);

function toPosix(path) {
  return path.replaceAll('\\', '/');
}

function walk(directory, files, skippedDirectories) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const absolute = join(directory, entry.name);
    if (entry.isDirectory()) {
      if (SKIPPED_DIRECTORIES.has(entry.name)) {
        skippedDirectories.push(`${toPosix(relative(ROOT, absolute))}/`);
      } else {
        walk(absolute, files, skippedDirectories);
      }
    } else if (SOURCE_EXTENSIONS.has(extname(entry.name))) {
      files.push(absolute);
    }
  }
}

function isTestFile(relativePath) {
  const segments = relativePath.split('/');
  if (segments.some((segment) => TEST_DIRECTORIES.has(segment))) return true;
  const basename = segments.at(-1);
  if (/\.(?:test|spec)\.ts$/.test(basename)) return true;
  if (/^tests?(?:[-_.]|$)/.test(basename)) return true;
  return /_tests?\.rs$/.test(basename);
}

function isGeneratedFile(relativePath) {
  return GENERATED_FILES.has(relativePath) || relativePath.endsWith('.generated.ts');
}

function testItemEnd(content, start) {
  let depth = 0;
  for (let i = start; i < content.length; i++) {
    const character = content[i];
    if (character === '/' && content[i + 1] === '/') {
      const newline = content.indexOf('\n', i + 2);
      if (newline === -1) return content.length;
      i = newline;
      continue;
    }
    if (character === '/' && content[i + 1] === '*') {
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
      character === 'r' || character === 'b' || character === 'c'
        ? content.slice(i).match(/^(?:r|br|cr)(#*)"/)
        : null;
    if (rawString) {
      const terminator = `"${rawString[1]}`;
      const close = content.indexOf(terminator, i + rawString[0].length);
      if (close === -1) return content.length;
      i = close + terminator.length - 1;
      continue;
    }
    if (character === '"') {
      i++;
      while (i < content.length && content[i] !== '"') {
        i += content[i] === '\\' ? 2 : 1;
      }
      continue;
    }
    const charLiteral =
      character === "'"
        ? content
            .slice(i)
            .match(/^'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|[\s\S])|[^\\'])'/u)
        : null;
    if (charLiteral) {
      i += charLiteral[0].length - 1;
      continue;
    }
    if (character === '{') {
      depth++;
    } else if (character === '}') {
      depth--;
      if (depth === 0) return i + 1;
    } else if (character === ';' && depth === 0) {
      return i + 1;
    }
  }
  return content.length;
}

function maskTestItems(content) {
  const attribute = /^[ \t]*#\[cfg\(test\)\]/gm;
  let masked = '';
  let copiedThrough = 0;
  let match;
  while ((match = attribute.exec(content)) !== null) {
    const start = match.index;
    const end = testItemEnd(content, start);
    masked += content.slice(copiedThrough, start);
    masked += content.slice(start, end).replace(/[^\r\n]/g, ' ');
    copiedThrough = end;
    attribute.lastIndex = end;
  }
  return masked + content.slice(copiedThrough);
}

function externalTestModuleFiles(files) {
  const result = new Set();
  const declaration = /^[ \t]*#\[cfg\(test\)\][ \t]*\r?\n[ \t]*mod\s+([A-Za-z0-9_]+)\s*;/gm;
  for (const file of files) {
    if (extname(file) !== '.rs') continue;
    const content = readFileSync(file, 'utf8');
    for (const match of content.matchAll(declaration)) {
      const directory = file.endsWith('mod.rs') ? dirname(file) : file.slice(0, -3);
      for (const candidate of [
        join(directory, `${match[1]}.rs`),
        join(directory, match[1], 'mod.rs'),
      ]) {
        if (existsSync(candidate)) result.add(candidate);
      }
    }
  }
  return result;
}

function maskRange(masked, start, end) {
  for (let i = start; i < end; i++) {
    if (masked[i] !== '\n' && masked[i] !== '\r') masked[i] = ' ';
  }
}

function regexCanStart(masked, index) {
  let cursor = index - 1;
  while (cursor >= 0 && /\s/.test(masked[cursor])) cursor--;
  if (cursor < 0) return true;
  if ('([{=,:;!&|?+-*%^~<>'.includes(masked[cursor])) return true;
  if (!/[A-Za-z0-9_$]/.test(masked[cursor])) return false;
  const end = cursor + 1;
  while (cursor >= 0 && /[A-Za-z0-9_$]/.test(masked[cursor])) cursor--;
  return REGEX_PREFIX_KEYWORDS.has(masked.slice(cursor + 1, end).join(''));
}

function skipQuotedString(content, index, quote) {
  let cursor = index + 1;
  while (cursor < content.length) {
    if (content[cursor] === '\\') {
      cursor += 2;
    } else if (content[cursor] === quote) {
      return cursor + 1;
    } else {
      cursor++;
    }
  }
  return content.length;
}

function skipRegex(content, index) {
  let inCharacterClass = false;
  let cursor = index + 1;
  while (cursor < content.length) {
    const character = content[cursor];
    if (character === '\\') {
      cursor += 2;
      continue;
    }
    if (character === '\n' || character === '\r') return index + 1;
    if (character === '[') inCharacterClass = true;
    if (character === ']') inCharacterClass = false;
    if (character === '/' && !inCharacterClass) {
      cursor++;
      while (cursor < content.length && /[A-Za-z]/.test(content[cursor])) cursor++;
      return cursor;
    }
    cursor++;
  }
  return content.length;
}

function maskComments(content, extension) {
  const masked = content.split('');
  const rust = extension === '.rs';
  let cursor = 0;
  while (cursor < content.length) {
    const character = content[cursor];
    const next = content[cursor + 1];
    if (character === '/' && next === '/') {
      let end = cursor + 2;
      while (end < content.length && content[end] !== '\n' && content[end] !== '\r') end++;
      maskRange(masked, cursor, end);
      cursor = end;
      continue;
    }
    if (character === '/' && next === '*') {
      let depth = 1;
      let end = cursor + 2;
      while (end < content.length && depth > 0) {
        if (rust && content.startsWith('/*', end)) {
          depth++;
          end += 2;
        } else if (content.startsWith('*/', end)) {
          depth--;
          end += 2;
        } else {
          end++;
        }
      }
      maskRange(masked, cursor, end);
      cursor = end;
      continue;
    }
    if (rust) {
      const rawString =
        character === 'r' || character === 'b' || character === 'c'
          ? content.slice(cursor).match(/^(?:r|br|cr)(#*)"/)
          : null;
      if (rawString) {
        const terminator = `"${rawString[1]}`;
        const close = content.indexOf(terminator, cursor + rawString[0].length);
        cursor = close === -1 ? content.length : close + terminator.length;
        continue;
      }
      if (character === "'") {
        const charLiteral = content
          .slice(cursor)
          .match(/^'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\}|[\s\S])|[^\\'])'/u);
        if (charLiteral) {
          cursor += charLiteral[0].length;
          continue;
        }
      }
    }
    if (character === '"' || (!rust && (character === "'" || character === '`'))) {
      cursor = skipQuotedString(content, cursor, character);
      continue;
    }
    if (!rust && character === '/' && regexCanStart(masked, cursor)) {
      cursor = skipRegex(content, cursor);
      continue;
    }
    cursor++;
  }
  return masked.join('');
}

function countCodeLines(content, extension) {
  const withoutComments = maskComments(content, extension);
  return withoutComments.split(/\r?\n/).reduce((count, line) => count + (line.trim() ? 1 : 0), 0);
}

function isPatch(relativePath) {
  return /^packages\/[^/]+\/src\/(?:.*\/)?patches\//.test(relativePath);
}

if (!existsSync(PACKAGES)) {
  console.error('check-patch-budget: packages/ directory not found');
  process.exit(2);
}

const files = [];
const skippedDirectories = [];
walk(PACKAGES, files, skippedDirectories);
files.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
skippedDirectories.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
const externalTests = externalTestModuleFiles(files);
const packageStats = new Map();
const excludedFiles = [];
const inlineTests = [];
let projectLines = 0;
let patchLines = 0;

for (const file of files) {
  const relativePath = toPosix(relative(ROOT, file));
  const extension = extname(file);
  const content = readFileSync(file, 'utf8');
  const packageName = relativePath.split('/')[1];
  if (!packageStats.has(packageName)) packageStats.set(packageName, { project: 0, patch: 0 });
  const stats = packageStats.get(packageName);

  let exclusion;
  if (isGeneratedFile(relativePath)) exclusion = 'generated';
  else if (externalTests.has(file)) exclusion = 'rust-cfg-test-module';
  else if (isTestFile(relativePath)) exclusion = 'test';
  if (exclusion) {
    excludedFiles.push({ path: relativePath, reason: exclusion, lines: countCodeLines(content, extension) });
    continue;
  }

  const withoutTests = extension === '.rs' ? maskTestItems(content) : content;
  const lines = countCodeLines(withoutTests, extension);
  if (extension === '.rs' && withoutTests !== content) {
    inlineTests.push({
      path: relativePath,
      lines: countCodeLines(content, extension) - lines,
    });
  }
  projectLines += lines;
  stats.project += lines;
  if (isPatch(relativePath)) {
    patchLines += lines;
    stats.patch += lines;
  }
}

const budget = Math.floor(projectLines * LIMIT);
const ratio = projectLines === 0 ? (patchLines === 0 ? 0 : Infinity) : patchLines / projectLines;
const passed = ratio <= LIMIT;
const percentage = Number.isFinite(ratio) ? `${(ratio * 100).toFixed(4)}%` : 'Infinity';

console.log(`check-patch-budget: ${passed ? 'OK' : 'FAIL'}`);
console.log(`Project code lines: ${projectLines}`);
console.log(`Patch code lines: ${patchLines}`);
console.log(`Patch ratio: ${percentage} (limit ${LIMIT * 100}%)`);
console.log(`Patch budget: ${budget} lines`);
console.log('Line metric: non-blank, non-comment physical lines; Rust #[cfg(test)] items excluded.');
console.log('Scope: packages/**/*.rs and packages/**/*.ts, including untracked working-tree files.');
console.log('Out of scope: apps/, reports/, normify-kimi-code/, scripts/, and all non-RS/non-TS files.');
console.log('\nPer-package code lines:');
for (const [packageName, stats] of [...packageStats].sort(([left], [right]) => left.localeCompare(right))) {
  console.log(`  ${packageName}: project=${stats.project}, patch=${stats.patch}`);
}

console.log(`\nExcluded directories (${skippedDirectories.length}):`);
for (const path of skippedDirectories) console.log(`  [dependency/build-output] ${path}`);
console.log(`\nExcluded files (${excludedFiles.length}):`);
for (const entry of excludedFiles) {
  console.log(`  [${entry.reason}] ${entry.path} (${entry.lines} code lines)`);
}
console.log(`\nExcluded inline Rust test items (${inlineTests.length}):`);
for (const entry of inlineTests) console.log(`  ${entry.path} (${entry.lines} code lines)`);

if (!passed) {
  console.error(`check-patch-budget: ${patchLines} patch lines exceed the ${budget}-line budget`);
  process.exitCode = 1;
}
