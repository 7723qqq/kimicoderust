// Zod mirrors of the payloads that cross the napi callback boundary as JSON
// strings (`rpc/types.rs` → the `host/*` callbacks). `napi-contract.d.ts` types
// the calls at compile time, but the payloads themselves travel as strings, so a
// Rust-side rename arrives as a missing property rather than a type error. These
// schemas are the only runtime check, and `session-handle.ts` runs them before
// handing the original bytes to the host callback (`checked`).
//
// Only payloads with a live callback are mirrored. This file used to also carry
// the turn/telemetry events, the stdio `session/*` params and results, and
// `agent/run_turn`: none of them has a consumer in `packages` or `apps` (measured
// by name — zero references outside this file), and the reference checkout has no
// counterpart file at all. They were deleted rather than wired, because a schema
// no call site runs is documentation that cannot fail.
import { z } from 'zod';

const toolDef = z.object({
  name: z.string(),
  description: z.string(),
  input_schema: z.unknown(),
});

export const llmChatRequestSchema = z.object({
  system_prompt: z.string(),
  model_name: z.string(),
  messages: z.array(
    z.object({
      role: z.string(),
      content: z.string(),
      blocks: z.array(z.unknown()).optional(),
    }),
  ),
  tools: z.array(toolDef),
  request_id: z.string().optional(),
});

export const toolExecuteRequestSchema = z.object({
  turn_id: z.string(),
  tool_call_id: z.string(),
  tool_name: z.string(),
  arguments: z.unknown(),
});

export const permissionCheckRequestSchema = z.object({
  tool_name: z.string(),
  tool_call_id: z.string(),
  arguments: z.unknown(),
});

// ── Rust→JS: OAuth token fetch (`host/auth_token`) ─────────────────────────
// The host owns the OAuth store (single-flight refresh); `force` asks it to
// refresh past the cache after a 401/403 from the provider.
export const authTokenRequestSchema = z.object({
  provider: z.string(),
  force: z.boolean(),
});

export const authTokenResponseSchema = z.object({
  token: z.string(),
});
