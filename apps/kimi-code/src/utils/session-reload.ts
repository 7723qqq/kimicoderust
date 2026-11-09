import { KimiError } from '@moonshot-ai/kimi-code-sdk';

/** Minimal shape needed to reload a session after another client wrote to it. */
export interface ReloadableSession {
  reloadSession(): Promise<unknown>;
}

export interface ReloadOnStaleOptions {
  /** Mirrors v2: `false` still reloads, then rethrows the stale error. */
  retry?: boolean;
  onReloaded?: (session: ReloadableSession) => Promise<void> | void;
}

const SESSION_STALE = 'session.stale';

/**
 * Run `run` and, if it fails because the session journal moved underneath us,
 * reload the session once (v2 `reloadOnStale`).
 *
 * The reload happens before the `retry` decision, matching v2: when
 * `retry: false` the caller still gets a reloaded session and the original
 * stale error. Any non-stale failure propagates unchanged.
 */
export async function reloadOnStale<T>(
  session: ReloadableSession,
  run: () => Promise<T>,
  options: ReloadOnStaleOptions = {},
): Promise<T> {
  try {
    return await run();
  } catch (error) {
    if (!(error instanceof KimiError) || error.code !== SESSION_STALE) {
      throw error;
    }
    await session.reloadSession();
    await options.onReloaded?.(session);
    if (options.retry === false) {
      throw error;
    }
  }
  return run();
}
