Run a TypeScript/JavaScript program in an isolated sandbox and return its console output and completion value.

- The program body runs as an async function: top-level `await` and `return` work (e.g. `return 1 + 2`, `await Promise.resolve(1)`).
- `console.log` / `console.info` / `console.warn` / `console.error` / `console.debug` output is captured in emission order.
- The completion value must be JSON-serializable (objects, arrays, strings, numbers, booleans, null). `undefined` (e.g. a bare `return`) reports `value: undefined`. Functions, classes, and circular structures fail with an `invalid-output` error.
- No state survives between calls — each run gets a fresh sandbox.
- The sandbox has a hard memory cap and execution timeout; a program that exceeds either fails with a `memory-limit` or `timeout` error and cannot take the host down with it.
- `require` is limited to `node:util`; the filesystem, process API, timers, and network are not available.
- Prefer this tool over Bash for pure computation, data transformation, algorithm verification, parsing, and small scripting tasks that do not need shell features.
