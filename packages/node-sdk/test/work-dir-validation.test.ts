import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterAll, describe, expect, it } from 'vitest';

import { assertUsableWorkDir } from '../src/native/sdk-client/helpers';

const root = mkdtempSync(join(tmpdir(), 'kimi-workdir-'));

afterAll(() => {
  rmSync(root, { recursive: true, force: true });
});

describe('createSession workDir validation', () => {
  it('accepts an existing directory', () => {
    const dir = join(root, 'project');
    mkdirSync(dir);
    expect(() => assertUsableWorkDir(dir)).not.toThrow();
  });

  it('reports a missing path as fs.path_not_found', () => {
    const missing = join(root, 'missing');
    expect(() => assertUsableWorkDir(missing)).toThrowError(
      expect.objectContaining({
        code: 'fs.path_not_found',
        message: `workspace root ${missing} does not exist`,
      }),
    );
  });

  it('reports a file as fs.path_not_found instead of accepting it', () => {
    const file = join(root, 'note.txt');
    writeFileSync(file, 'x');
    expect(() => assertUsableWorkDir(file)).toThrowError(
      expect.objectContaining({
        code: 'fs.path_not_found',
        message: `workspace root ${file} is not a directory`,
      }),
    );
  });
});
