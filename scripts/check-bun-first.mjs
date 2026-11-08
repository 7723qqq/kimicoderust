#!/usr/bin/env bun
/**
 * Bun-first policy gate.
 *
 * The product ships on Bun (`engines: bun >= 1.4`), so three things hold:
 *
 * 1. **Nothing starts on Node.** A `#!/usr/bin/env node` shebang, or a spawned
 *    `node` command, runs code on a runtime the package does not support.
 * 2. **A Bun-native API wins where one exists.** `Bun.serve` / `Bun.listen` /
 *    `Bun.zstd*` are implemented natively; the `node:` module behind them is a
 *    compatibility surface. Only those modules are gated, because that is where
 *    the ordering is a real choice.
 * 3. **Otherwise `node:` is the right answer, not a compromise.** Bun
 *    implements `node:fs` / `node:path` / `node:child_process` in Rust; using
 *    them *is* the Bun path, so they are deliberately not gated here.
 * 4. **The suite runs under Bun** — verifying on Node hides exactly the
 *    differences this policy exists for.
 *
 * Each rule is a ratchet: an allowlist entry records the debt with a reason, an
 * entry that is gone fails, and a new violation fails. `--update` re-records.
 */
const ROOT = new URL('..', import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1');
const ALLOWLIST = 'scripts/bun-first-allowlist.json';

/** `node:` modules with a Bun-native API that is strictly better. */
const NATIVE_API_MODULES = new Set(['http', 'https', 'net', 'zlib']);

const TEST_SCRIPT_PREFIX = /^(?:bun --bun run |bun --bun )/;

const SHEBANG_GLOBS = [
  'packages/*/src/**/*.{ts,mts,mjs}',
  'apps/*/src/**/*.{ts,mts,mjs}',
  'plugins/**/*.mjs',
  'scripts/**/*.{mjs,cjs,ts}',
];

const SOURCE_GLOBS = [
  'packages/*/src/**/*.{ts,tsx,mts}',
  'apps/*/src/**/*.{ts,tsx,mts}',
  'apps/vis/*/src/**/*.ts',
];

/** Where a command named `node` would actually be executed. */
const SPAWN_GLOBS = ['plugins/official/**/kimi.plugin.json'];
const SPAWN_LITERAL =
  /(?:spawn|spawnSync|exec|execFile|execFileSync)\(\s*['"]node(?:\.exe)?['"]|"command"\s*:\s*"node"/;

const NODE_IMPORT = /(?:from|require\()\s*['"]node:([a-z_]+)['"]/g;

async function readJson(path) {
  try {
    return JSON.parse(await Bun.file(`${ROOT}/${path}`).text());
  } catch {
    return {};
  }
}

async function scanFiles(globs) {
  const paths = new Set(); // oxlint treats generics in .mjs as parse errors
  for (const pattern of globs) {
    const glob = new Bun.Glob(pattern);
    for await (const path of glob.scan({ cwd: ROOT, dot: false })) {
      paths.add(path.replaceAll('\\', '/'));
    }
  }
  return [...paths].sort((a, b) => a < b ? -1 : a > b ? 1 : 0);
}

async function shebangViolations() {
  const found = [];
  for (const path of await scanFiles(SHEBANG_GLOBS)) {
    const first = (await Bun.file(`${ROOT}/${String(path)}`).text()).split('\n', 1)[0] ?? '';
    if (/^#!.*\bnode\b/.test(first)) found.push(path);
  }
  return found;
}

async function nativeApiViolations() {
  const found = [];
  for (const path of await scanFiles(SOURCE_GLOBS)) {
    const text = await Bun.file(`${ROOT}/${String(path)}`).text();
    const modules = new Set();
    for (const match of text.matchAll(NODE_IMPORT)) {
      const module = match[1];
      if (NATIVE_API_MODULES.has(module)) modules.add(module);
    }
    for (const module of [...modules].sort((a, b) => String(a).localeCompare(String(b)))) found.push(`${String(path)}::${String(module)}`);
  }
  return found;
}

async function nodeStartViolations() {
  const found = [];
  for (const path of await scanFiles(SPAWN_GLOBS)) {
    const text = await Bun.file(`${ROOT}/${String(path)}`).text();
    if (SPAWN_LITERAL.test(text)) found.push(path);
  }
  return found;
}

async function testScriptViolations() {
  const manifest = await readJson('package.json');
  const found = [];
  for (const [name, command] of Object.entries(manifest.scripts ?? {})) {
    if (!/^test(?::|$)/.test(name)) continue;
    if (typeof command !== 'string') continue;
    if (!command.includes('vitest')) continue;
    if (!TEST_SCRIPT_PREFIX.test(command)) found.push(`${name}: ${command}`);
  }
  return found;
}

const allowlist = await readJson(ALLOWLIST);
const groups = {
  shebang: await shebangViolations(),
  nativeApi: await nativeApiViolations(),
  nodeStart: await nodeStartViolations(),
  testScripts: await testScriptViolations(),
};

const unrecorded = [];
for (const rule of ['shebang', 'nativeApi', 'nodeStart']) {
  for (const key of groups[rule]) {
    if (allowlist[rule]?.[key] === undefined) unrecorded.push(`${rule}: ${key}`);
  }
}
for (const entry of groups.testScripts) {
  if (allowlist.testScripts?.[entry] === undefined) unrecorded.push(`testScripts: ${entry}`);
}

const stale = [];
for (const rule of ['shebang', 'nativeApi', 'nodeStart', 'testScripts']) {
  for (const key of Object.keys(allowlist[rule] ?? {})) {
    if (!groups[rule].includes(key)) stale.push(`${rule}: ${key}`);
  }
}

if (process.argv.includes('--update')) {
  const reason = 'TODO: record why this cannot use the Bun-native path yet';
  const next = {};
  for (const rule of ['shebang', 'nativeApi', 'nodeStart', 'testScripts']) {
    next[rule] = Object.fromEntries(
      groups[rule].map((key) => [key, allowlist[rule]?.[key] ?? reason]),
    );
  }
  await Bun.write(`${ROOT}/${ALLOWLIST}`, `${JSON.stringify(next, null, 2)}\n`);
  console.log(
    `check-bun-first: allowlist refreshed (${groups.shebang.length} shebang, ${groups.nativeApi.length} native api, ${groups.nodeStart.length} node start, ${groups.testScripts.length} test script)`,
  );
  process.exit(0);
}

if (stale.length > 0) {
  console.error('check-bun-first: stale allowlist entries — the code moved on, drop them:');
  for (const entry of stale) console.error(`  - ${entry}`);
}
if (unrecorded.length > 0) {
  console.error(`check-bun-first: unrecorded Node path — fix it or record why in ${ALLOWLIST}:`);
  for (const entry of unrecorded) console.error(`  - ${entry}`);
  console.error(
    "\nPolicy: Bun native API first; otherwise Bun's Rust implementation of the node: module (not a Node process); nothing starts on Node.",
  );
}
if (stale.length > 0 || unrecorded.length > 0) process.exit(1);

console.log(
  `check-bun-first: ${groups.shebang.length} shebang, ${groups.nativeApi.length} native-api, ${groups.nodeStart.length} node-start, ${groups.testScripts.length} test-script recorded — no unrecorded Node path`,
);
