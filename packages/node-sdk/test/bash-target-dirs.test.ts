import { describe, expect, it } from 'vitest';

import { bashTargetDirs } from '../src/native/sdk-client/bash-target-dirs';

// The walker behind `host/parse_bash` (v2 `extractBashTargetDirs`). Cases
// mirror v2's "agentsMdReminder Bash coverage" block, plus the failure arm
// the engine's cwd-only fallback rides on. Paths stay posix-shaped so the
// expectations hold on every platform pathe runs on.
const CWD = '/repo';
const HOME = '/home/user';

describe('bashTargetDirs', () => {
  it('resolves a plain ls operand against the cwd', () => {
    expect(bashTargetDirs('ls sub', CWD, HOME)).toEqual(['/repo/sub']);
  });

  it('rebases relative operands across a literal cd', () => {
    expect(bashTargetDirs('cd sub && ls other', CWD, HOME)).toEqual(['/repo/sub/other']);
  });

  it('extracts find roots and stops at the expression', () => {
    expect(bashTargetDirs('find src tests -name "*.ts"', CWD, HOME)).toEqual([
      '/repo/src',
      '/repo/tests',
    ]);
  });

  it('extracts quoted directory operands', () => {
    expect(bashTargetDirs('ls "some dir"', CWD, HOME)).toEqual(['/repo/some dir']);
    expect(bashTargetDirs("ls 'some dir'", CWD, HOME)).toEqual(['/repo/some dir']);
  });

  it('keeps absolute operands absolute', () => {
    expect(bashTargetDirs('ls /etc', CWD, HOME)).toEqual(['/etc']);
    expect(bashTargetDirs('cd /abs && ls rel', CWD, HOME)).toEqual(['/abs/rel']);
  });

  it('skips operands that are not statically resolvable', () => {
    expect(bashTargetDirs('ls $DIR', CWD, HOME)).toEqual([]);
    expect(bashTargetDirs('ls *.ts', CWD, HOME)).toEqual([]);
    expect(bashTargetDirs('cd $X && ls sub', CWD, HOME)).toEqual([]);
  });

  it('reads a bare cd as the home directory', () => {
    expect(bashTargetDirs('cd && ls sub', CWD, HOME)).toEqual(['/home/user/sub']);
  });

  it('drops flags, including argument-taking ones', () => {
    expect(bashTargetDirs('ls -la sub', CWD, HOME)).toEqual(['/repo/sub']);
    expect(bashTargetDirs('ls --sort size sub', CWD, HOME)).toEqual(['/repo/sub']);
    expect(bashTargetDirs('ls --sort=size sub', CWD, HOME)).toEqual(['/repo/sub']);
    expect(bashTargetDirs('tree -L 2 sub', CWD, HOME)).toEqual(['/repo/sub']);
  });

  it('sees through pipelines and redirections', () => {
    expect(bashTargetDirs('ls sub | grep x', CWD, HOME)).toEqual(['/repo/sub']);
    // The redirect target is not an operand of `ls`.
    expect(bashTargetDirs('ls sub > out.txt', CWD, HOME)).toEqual(['/repo/sub']);
  });

  it('dedupes repeated targets in first-seen order', () => {
    expect(bashTargetDirs('ls sub sub src', CWD, HOME)).toEqual(['/repo/sub', '/repo/src']);
  });

  it('contributes the cwd itself for a bare listing command', () => {
    expect(bashTargetDirs('ls', CWD, HOME)).toEqual(['/repo']);
  });

  it('skips commands whose name is not a literal or carries a path', () => {
    expect(bashTargetDirs('/bin/ls sub', CWD, HOME)).toEqual([]);
    expect(bashTargetDirs('$TOOL sub', CWD, HOME)).toEqual([]);
  });

  it('treats number operands as literal', () => {
    expect(bashTargetDirs('ls 123', CWD, HOME)).toEqual(['/repo/123']);
  });

  it('returns null for a command that does not parse', () => {
    // The failure arm the engine's cwd-only fallback rides on
    // (v2 `!parsed.ok || parsed.hasError`).
    expect(bashTargetDirs('ls "unbalanced', CWD, HOME)).toBeNull();
  });
});
