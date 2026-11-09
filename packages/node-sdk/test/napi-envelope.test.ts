import { describe, expect, it } from 'vitest';

import { KimiError, decodeNapiEnvelope, normalizeNapiError, wrapNapiHandle } from '#/error-protocol';

const MARKER = '\n[kimi-error] ';

function envelope(message: string, code: string, details?: Record<string, unknown>): Error {
  return new Error(`${message}${MARKER}${JSON.stringify({ code, message, details })}`);
}

describe('napi error envelope', () => {
  it('decodes the engine code, message and details', () => {
    const decoded = decodeNapiEnvelope(
      envelope('unknown session: s1', 'session.not_found', { sessionId: 's1' }),
    );
    expect(decoded).toBeInstanceOf(KimiError);
    expect(decoded?.code).toBe('session.not_found');
    expect(decoded?.message).toBe('unknown session: s1');
    expect(decoded?.details).toEqual({ sessionId: 's1' });
  });

  it('keeps a declared code that only the engine knows about', () => {
    const decoded = decodeNapiEnvelope(envelope('stale view', 'session.stale'));
    expect(decoded?.code).toBe('session.stale');
  });

  it('degrades an unknown code to internal without dropping details', () => {
    const decoded = decodeNapiEnvelope(
      envelope('future failure', 'engine.not_in_this_build', { attempt: 2 }),
    );
    expect(decoded?.code).toBe('internal');
    expect(decoded?.details).toEqual({ attempt: 2 });
  });

  it('leaves plain errors and KimiErrors untouched', () => {
    const plain = new Error('plain failure');
    const kimi = new KimiError('session.busy', 'busy');
    expect(decodeNapiEnvelope(plain)).toBeUndefined();
    expect(normalizeNapiError(plain)).toBe(plain);
    expect(normalizeNapiError(kimi)).toBe(kimi);
  });

  it('normalizes sync throws and rejected promises from a wrapped handle', async () => {
    const handle = wrapNapiHandle({
      sync(): never {
        throw envelope('sync boom', 'session.closed');
      },
      async asyncThrow(): Promise<never> {
        throw envelope('async boom', 'session.stale');
      },
      value: 7,
    });
    expect(handle.value).toBe(7);
    expect(() => handle.sync()).toThrow(KimiError);
    await expect(handle.asyncThrow()).rejects.toMatchObject({
      code: 'session.stale',
      message: 'async boom',
    });
  });
});
