/*
 * `run_code` prelude. Loaded through `include_str!` and evaluated once per
 * call in a fresh QuickJS runtime.
 *
 * The program body replaces `/*__RUN_CODE_BODY__*\/` below. It runs as an
 * async function, so top-level `await` and `return` work (`v2`
 * `codeShimSource.ts` builds the same wrapper with `AsyncFunction`).
 *
 * Deliberately absent: `process`, timers, filesystem modules. The only
 * `require` names in the allowlist are the pure `node:util` helpers the
 * console renderer already needs. Anything else is refused before it can
 * reach the host.
 */
(async () => {
  'use strict';

  const __util = (() => {
    const INSPECT_DEPTH = 4;
    const MAX_ARRAY_LENGTH = 100;
    const MAX_STRING_LENGTH = 10000;

    function inspect(value, depth = 0, seen = new Set()) {
      const type = typeof value;
      if (type === 'string') {
        if (depth === 0) return value;
        const text = value.length > MAX_STRING_LENGTH
          ? value.slice(0, MAX_STRING_LENGTH) + '…'
          : value;
        return JSON.stringify(text);
      }
      if (value === undefined) return 'undefined';
      if (value === null) return 'null';
      if (type === 'number' || type === 'boolean' || type === 'bigint') return String(value);
      if (type === 'symbol') return String(value);
      if (type === 'function') return '[Function: ' + (value.name || 'anonymous') + ']';
      if (value instanceof Error) return value.stack || (value.name + ': ' + value.message);
      if (depth >= INSPECT_DEPTH) return Array.isArray(value) ? '[Array]' : '[Object]';
      if (seen.has(value)) return '[Circular]';
      seen.add(value);
      try {
        if (Array.isArray(value)) {
          const items = value.slice(0, MAX_ARRAY_LENGTH).map((item) => inspect(item, depth + 1, seen));
          if (value.length > MAX_ARRAY_LENGTH) {
            items.push('... ' + (value.length - MAX_ARRAY_LENGTH) + ' more items');
          }
          return '[ ' + items.join(', ') + ' ]';
        }
        if (value instanceof Date) return value.toISOString();
        if (value instanceof Map) {
          return 'Map(' + value.size + ') ' + inspect(Array.from(value.entries()), depth + 1, seen);
        }
        if (value instanceof Set) {
          return 'Set(' + value.size + ') ' + inspect(Array.from(value.values()), depth + 1, seen);
        }
        const keys = Object.keys(value);
        const parts = keys.slice(0, MAX_ARRAY_LENGTH).map((key) => {
          return key + ': ' + inspect(value[key], depth + 1, seen);
        });
        if (keys.length > MAX_ARRAY_LENGTH) {
          parts.push('... ' + (keys.length - MAX_ARRAY_LENGTH) + ' more keys');
        }
        return '{ ' + parts.join(', ') + ' }';
      } catch {
        return String(value);
      } finally {
        seen.delete(value);
      }
    }

    function format(...args) {
      if (typeof args[0] !== 'string') return args.map((arg) => inspect(arg)).join(' ');
      let index = 1;
      let rendered = args[0].replace(/%[sdifjoO%]/g, (match) => {
        if (match === '%%') return '%';
        if (index >= args.length) return match;
        const arg = args[index++];
        if (match === '%d' || match === '%i') return String(parseInt(arg, 10));
        if (match === '%f') return String(parseFloat(arg));
        if (match === '%j') {
          try {
            return JSON.stringify(arg);
          } catch {
            return '[Circular]';
          }
        }
        if (match === '%s') return String(arg);
        return inspect(arg);
      });
      while (index < args.length) rendered += ' ' + inspect(args[index++]);
      return rendered;
    }

    function isDeepStrictEqual(left, right) {
      if (left === right) return true;
      if (typeof left !== 'object' || typeof right !== 'object' || left === null || right === null) {
        return false;
      }
      const leftKeys = Object.keys(left);
      const rightKeys = Object.keys(right);
      if (leftKeys.length !== rightKeys.length) return false;
      return leftKeys.every((key) => {
        return Object.prototype.hasOwnProperty.call(right, key)
          && isDeepStrictEqual(left[key], right[key]);
      });
    }

    return { format, inspect, isDeepStrictEqual };
  })();

  const __modules = Object.create(null);
  __modules['util'] = __util;
  __modules['node:util'] = __util;
  const __allowedModules = 'node:util';

  globalThis.require = function require(name) {
    const key = String(name);
    if (Object.prototype.hasOwnProperty.call(__modules, key)) return __modules[key];
    throw new Error(
      "module '" + key + "' is not available in run_code; allowed modules: " + __allowedModules
    );
  };

  const __levels = ['log', 'info', 'warn', 'error', 'debug'];
  const __console = {};
  for (const level of __levels) {
    __console[level] = (...args) => {
      __native_log(args.map((arg) => (typeof arg === 'string' ? arg : __util.inspect(arg))).join(' '));
    };
  }
  globalThis.console = __console;

  try {
    const __value = await (async () => {
      'use strict';
      /*__RUN_CODE_BODY__*/
    })();

    if (__value === undefined) {
      return JSON.stringify({ ok: true, hasValue: false });
    }

    let __json;
    try {
      __json = JSON.stringify(__value);
    } catch {
      __json = undefined;
    }
    if (__json === undefined) {
      return JSON.stringify({
        ok: false,
        kind: 'invalid-output',
        message: 'program completion must be JSON-serializable',
      });
    }
    return JSON.stringify({ ok: true, hasValue: true, json: __json });
  } catch (__error) {
    let __message;
    try {
      if (__error instanceof Error) {
        const __name = __error.name ? String(__error.name) : 'Error';
        const __text = __error.message !== undefined ? String(__error.message) : '';
        const __stack = typeof __error.stack === 'string' ? __error.stack : '';
        __message = __name + ': ' + __text;
        if (__stack !== '' && __stack.indexOf(__text) === -1) {
          __message += '\n' + __stack;
        } else if (__stack !== '' && __message.indexOf(__stack) === -1) {
          __message += '\n' + __stack;
        }
      } else {
        __message = String(__error);
      }
    } catch {
      __message = 'program threw an unrenderable value';
    }
    const __kind = typeof __message === 'string' && /out of memory/i.test(__message)
      ? 'memory-limit'
      : 'exception';
    return JSON.stringify({ ok: false, kind: __kind, message: __message });
  }
})().catch((__error) => {
  throw __error;
})
