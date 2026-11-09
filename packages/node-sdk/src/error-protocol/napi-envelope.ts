import { type KimiErrorCode, isKimiErrorCode } from './error-codes';
import { KimiError } from './kimi-error';

/**
 * The marker the Rust engine appends before the machine-readable half of a
 * boundary error. Keep in sync with `MARKER` in
 * `packages/kimi-agent/src/napi_bindings/errors.rs`.
 */
const MARKER = '\n[kimi-error] ';

interface NapiEnvelope {
  readonly code?: unknown;
  readonly message?: string;
  readonly details?: unknown;
}

/**
 * Read the engine's `[kimi-error]` envelope out of an error raised across the
 * napi boundary.
 *
 * napi-rs flattens everything into `Error.message`, so without this step the
 * domain code and details only exist as text. Returns `undefined` when the
 * value carries no envelope (or an unparseable one), so callers can pass the
 * original error through unchanged.
 */
export function decodeNapiEnvelope(error: unknown): KimiError | undefined {
  if (error instanceof KimiError) {
    return error;
  }
  const raw =
    error instanceof Error ? error.message : typeof error === 'string' ? error : undefined;
  if (!raw) {
    return undefined;
  }
  const at = raw.lastIndexOf(MARKER);
  if (at < 0) {
    return undefined;
  }
  let parsed: NapiEnvelope;
  try {
    parsed = JSON.parse(raw.slice(at + MARKER.length)) as NapiEnvelope;
  } catch {
    return undefined;
  }
  const code: KimiErrorCode =
    typeof parsed.code === 'string' && isKimiErrorCode(parsed.code) ? parsed.code : 'internal';
  const message = raw.slice(0, at);
  const details =
    parsed.details !== null && typeof parsed.details === 'object' && !Array.isArray(parsed.details)
      ? (parsed.details as Record<string, unknown>)
      : undefined;
  return new KimiError(code, message === '' ? String(parsed.message ?? '') : message, {
    details,
    cause: error,
  });
}

/** Convert a boundary error into a `KimiError` when it carries an envelope. */
export function normalizeNapiError(error: unknown): unknown {
  return decodeNapiEnvelope(error) ?? error;
}

/**
 * Wrap an engine handle so every method preserves the engine's error code.
 *
 * Sync throws and rejected promises are both normalized; non-function
 * properties pass through untouched. The receiver stays the real handle, so
 * private state keeps working and `instanceof` checks remain valid.
 */
export function wrapNapiHandle<T extends object>(handle: T): T {
  return new Proxy(handle, {
    get(target, property, receiver) {
      const value = Reflect.get(target, property, receiver) as unknown;
      if (typeof value !== 'function') {
        return value;
      }
      return (...args: unknown[]) => {
        try {
          const result = (value as (...inner: unknown[]) => unknown).apply(target, args);
          if (result !== null && typeof (result as PromiseLike<unknown>).then === 'function') {
            return Promise.resolve(result).catch((error: unknown) => {
              throw normalizeNapiError(error);
            });
          }
          return result;
        } catch (error) {
          throw normalizeNapiError(error);
        }
      };
    },
  });
}
