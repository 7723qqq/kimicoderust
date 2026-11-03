// Terminal-state gate for the Rust engine migration (ROADMAP P161): fails
// when any retired engine package is still referenced from a live surface —
// dependency manifests, import/require specifiers, tsconfig program inputs,
// or the build/CI wiring. Prose mentions in comments and historical records
// (ROADMAP*.md, .changeset/, CHANGELOGs) are intentionally out of scope.
import { execFileSync } from 'node:child_process';
import { readdirSync, readFileSync } from 'node:fs';
import { join, relative, sep } from 'node:path';

const RETIRED_PACKAGES = [
  'agent-core-v2',
  'kimi-native-tools',
  'klient',
  'acp-server',
  'kap-server',
];

const SPECIFIER_PATTERN = new RegExp(
  `['"]@moonshot-ai/(${RETIRED_PACKAGES.join('|')})['"]`,
);
const DEP_ENTRY_PATTERN = new RegExp(
  `["']@moonshot-ai/(${RETIRED_PACKAGES.join('|')})["']\\s*:`,
);
const PATH_PATTERN = new RegExp(
  `packages/(${RETIRED_PACKAGES.join('|')})(/|['"\\s)]|$)`,
);

const CODE_EXTENSIONS = new Set(['.ts', '.tsx', '.mts', '.mjs', '.rs', '.vue']);
const SKIP_DIRS = new Set([
  'node_modules',
  'target',
  'dist',
  'dist-web',
  'dist-native',
  'coverage',
  '.changeset',
  'docs',
  'graphify-out',
  '.tmp',
  '.worktrees',
  '.git',
  '.kimi',
  '.agents',
  '.arts',
  '.qoder',
  '.zcode',
  '.codeartsdoer',
  '.codegraph',
  '.workbuddy',
  'reports',
]);

const SKIP_FILES = new Set([
  // Golden generator: shims retired-package specs via `build.module(...)` to
  // isolate v2's events-map.ts for golden recording. The specifiers are shim
  // targets, not runtime dependencies, so they are not "live surface"
  // references the gate exists to catch.
  'packages/kimi-agent/scripts/gen-acp-golden.mjs',
]);

function* walk(dir) {
  for (const entry of readdir(dir)) {
    if (SKIP_DIRS.has(entry.name)) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      yield* walk(full);
    } else {
      yield full;
    }
  }
}

function readdir(dir) {
  try {
    // Dirents carry the type: `statSync` per entry is the whole cost of this
    // walk on Windows, and the gate runs inside a 5 s test budget.
    return readdirSync(dir, { withFileTypes: true });
  } catch {
    return [];
  }
}

const violations = [];

/** Paths whose any segment is a skipped directory (the walk's rule). */
function isSkippedPath(rel) {
  return rel.split('/').some((segment) => SKIP_DIRS.has(segment));
}

/**
 * The live surface, from git: tracked plus untracked-not-ignored files. A
 * whole-tree walk costs seconds on Windows and this gate runs five times inside
 * a 5 s test budget; gitignored output is not a live surface, so it is not lost.
 * The walk stays as a fallback for an export without a `.git` directory.
 */
function listFiles() {
  try {
    const out = execFileSync('git', ['ls-files', '--cached', '--others', '--exclude-standard'], {
      cwd: '.',
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    });
    return out.split('\n').filter(Boolean);
  } catch {
    return [...walk('.')].map((file) => relative('.', file).split(sep).join('/'));
  }
}

for (const rel of listFiles()) {
  if (isSkippedPath(rel)) continue;
  if (rel.endsWith('.md') || rel.endsWith('.mdx')) continue;
  if (SKIP_FILES.has(rel)) continue;
  const file = rel;
  const base = file.split('/').pop() ?? '';
  const ext = base.split('.').pop() ?? '';
  const isCode = CODE_EXTENSIONS.has(`.${ext}`) || base.endsWith('.d.ts');
  const isBuildWiring =
    base.startsWith('tsconfig') ||
    base === 'flake.nix' ||
    rel.startsWith('.github/workflows/') ||
    base === 'bun.lock';
  // Skip before reading: the walk reaches binaries, images and `.node` addons
  // whose contents no pattern below can match, and reading them is the whole
  // cost of this gate.
  if (base !== 'package.json' && !isCode && !isBuildWiring) continue;
  // `git ls-files --cached` still lists files whose deletion is not yet staged;
  // they are gone from the live surface, so skip them instead of crashing.
  let text;
  try {
    text = readFileSync(file, 'utf8');
  } catch {
    continue;
  }

  if (base === 'package.json') {
    if (DEP_ENTRY_PATTERN.test(text)) {
      violations.push(`${rel}: dependency entry on a retired package`);
    }
    continue;
  }

  if (isCode) {
    if (SPECIFIER_PATTERN.test(text)) {
      violations.push(`${rel}: retired package specifier in code`);
    }
    continue;
  }

  if (isBuildWiring) {
    const pathMatch = text.match(PATH_PATTERN);
    if (pathMatch) {
      violations.push(`${rel}: build/config reference to packages/${pathMatch[1]}`);
    }
  }
}

if (violations.length > 0) {
  console.error(`check-no-legacy-engine: ${violations.length} retired-engine reference(s):`);
  for (const violation of violations) console.log(`  - ${violation}`);
  process.exit(1);
}
console.log('check-no-legacy-engine: no retired-engine references in live surfaces.');