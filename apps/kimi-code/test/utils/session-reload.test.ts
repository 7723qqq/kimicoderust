import { KimiError } from '@moonshot-ai/kimi-code-sdk';
import { describe, expect, it, vi } from 'vitest';

import { reloadOnStale } from '../../src/utils/session-reload';

function stale(): KimiError {
  return new KimiError('session.stale', 'session journal changed');
}

describe('reloadOnStale', () => {
  it('reloads once and retries the operation', async () => {
    const reloadSession = vi.fn(async () => undefined);
    const run = vi.fn().mockRejectedValueOnce(stale()).mockResolvedValueOnce('done');

    await expect(reloadOnStale({ reloadSession }, run)).resolves.toBe('done');
    expect(reloadSession).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledTimes(2);
  });

  it('reports the reload before retrying', async () => {
    const order: string[] = [];
    const run = vi
      .fn()
      .mockRejectedValueOnce(stale())
      .mockImplementationOnce(async () => {
        order.push('retry');
        return 'done';
      });

    await reloadOnStale(
      { reloadSession: async () => void order.push('reload') },
      run,
      { onReloaded: () => void order.push('callback') },
    );

    expect(order).toEqual(['reload', 'callback', 'retry']);
  });

  it('still reloads when retry is false, then rethrows the stale error', async () => {
    const reloadSession = vi.fn(async () => undefined);
    const run = vi.fn().mockRejectedValue(stale());

    await expect(reloadOnStale({ reloadSession }, run, { retry: false })).rejects.toMatchObject({
      code: 'session.stale',
    });
    expect(reloadSession).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('leaves other errors untouched', async () => {
    const reloadSession = vi.fn(async () => undefined);
    const failure = new KimiError('session.busy', 'busy');

    await expect(reloadOnStale({ reloadSession }, async () => Promise.reject(failure))).rejects.toBe(
      failure,
    );
    expect(reloadSession).not.toHaveBeenCalled();
  });
});
