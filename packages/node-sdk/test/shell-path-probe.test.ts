import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { probeShellPath } from '#/native/native-llm-resolver';

// `probeShellPath` mirrors the engine's own resolution
// (`packages/kimi-agent/src/native/shell.rs`, `resolve_shell`) on Windows:
// pwsh -> powershell -> Git Bash -> cmd. The same order is written twice, in
// two languages, and nothing makes the two halves fail together — if the host
// answers with Git Bash while the engine runs pwsh, the prompt's shell note
// disagrees with the shell the tool actually runs. This file pins the host half.
//
// The PATH lookup is `Bun.which` (native, no `where` subprocess), so the tests
// stub that one function instead of a child process. `Bun.which` is a writable
// property on the global (configurable: false, writable: true), which is what
// makes this possible without a seam in production code.
declare const Bun: {
  which(
    command: string,
    options?: { readonly PATH?: string; readonly cwd?: string },
  ): string | null;
};

const originalWhich = Bun.which;
let queried: string[] = [];

/** Answer `which <name>` for exactly the names in `found`. */
function answerWhich(found: Record<string, string>): void {
  queried = [];
  Bun.which = (command: string) => {
    queried.push(command);
    return found[command] ?? null;
  };
}

const PLATFORM = Object.getOwnPropertyDescriptor(process, 'platform');

function setPlatform(value: NodeJS.Platform): void {
  Object.defineProperty(process, 'platform', { value, configurable: true });
}

describe('probeShellPath', () => {
  beforeEach(() => {
    queried = [];
    delete process.env['KIMI_SHELL_PATH'];
    delete process.env['SHELL'];
  });

  afterEach(() => {
    Bun.which = originalWhich;
    if (PLATFORM) Object.defineProperty(process, 'platform', PLATFORM);
    delete process.env['KIMI_SHELL_PATH'];
    delete process.env['SHELL'];
  });

  it('lets KIMI_SHELL_PATH win without probing the platform', () => {
    setPlatform('win32');
    process.env['KIMI_SHELL_PATH'] = 'C:\\tools\\my-shell.exe';

    expect(probeShellPath()).toBe('C:\\tools\\my-shell.exe');
    expect(queried).toEqual([]);
  });

  it('prefers pwsh over powershell', () => {
    setPlatform('win32');
    answerWhich({
      'pwsh.exe': 'C:\\Program Files\\PowerShell\\7\\pwsh.exe',
      'powershell.exe': 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe',
    });

    expect(probeShellPath()).toBe('C:\\Program Files\\PowerShell\\7\\pwsh.exe');
    expect(queried).toEqual(['pwsh.exe']);
  });

  it('falls back to powershell when pwsh is absent, probing in that order', () => {
    setPlatform('win32');
    answerWhich({
      'powershell.exe': 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe',
    });

    expect(probeShellPath()).toBe('C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe');
    expect(queried).toEqual(['pwsh.exe', 'powershell.exe']);
  });

  it('uses SHELL, or /bin/bash, off Windows', () => {
    setPlatform('linux');

    expect(probeShellPath()).toBe('/bin/bash');

    process.env['SHELL'] = '/usr/bin/zsh';
    expect(probeShellPath()).toBe('/usr/bin/zsh');
    expect(queried).toEqual([]);
  });
});
