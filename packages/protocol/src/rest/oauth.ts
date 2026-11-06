/**
 *   POST   /v1/oauth/login   body: { provider?, region? }   data: OAuthFlowStart
 *   GET    /v1/oauth/login   query: { provider? }  data: OAuthFlowStatus | null
 *   DELETE /v1/oauth/login   query: { provider? }  data: { cancelled, status }
 *   POST   /v1/oauth/logout  body: { provider? }   data: { logged_out, provider }
 *   GET    /v1/oauth/usage   query: { provider? }  data: OAuthUsageResponse
 *   GET    /v1/oauth/region  -                   data: OAuthRegionResponse
 *   GET    /v1/oauth/user    query: { provider? }  data: OAuthUserInfoResponse
 *   GET    /v1/oauth/userinfo query: { provider? } data: OAuthUserInfoResponse
 *                                                       (alias of /user)
 */
import { z } from 'zod';

import { isoDateTimeSchema } from '../time';

export const oauthFlowStatusEnum = z.enum([
  'pending',
  'authenticated',
  'denied',
  'expired',
  'cancelled',
]);
export type OAuthFlowStatus = z.infer<typeof oauthFlowStatusEnum>;

export const oauthLoginStartRequestSchema = z.object({
  provider: z.string().min(1).optional(),
  // Region hint for the login flow (kap-server `oauthLoginStartRequestSchema`;
  // the fork's handler reads it the same way).
  region: z.enum(['mainland-cn', 'global']).optional(),
});
export type OAuthLoginStartRequest = z.infer<typeof oauthLoginStartRequestSchema>;

/**
 * Result of `POST /v1/oauth/login`.
 *
 * Two shapes, discriminated by `status`:
 *   - `pending`: a real device-code flow was started; the `verification_*`,
 *     `user_code`, `expires_*`, and `interval` fields are populated so the
 *     client can render the device-code step and start polling.
 *   - `authenticated`: the toolkit already had a usable token and short-
 *     circuited via its `ensureFresh` fast path, so no device code was
 *     issued. The client can skip the device-code step and treat the login
 *     as already complete.
 */
export const oauthFlowStartPendingSchema = z.object({
  flow_id: z.string().min(1),
  provider: z.string().min(1),
  status: z.literal('pending'),
  verification_uri: z.string().url(),
  verification_uri_complete: z.string().url(),
  user_code: z.string().min(1),
  expires_in: z.number().int().positive(),
  interval: z.number().int().positive(),
  expires_at: isoDateTimeSchema,
});
export type OAuthFlowStartPending = z.infer<typeof oauthFlowStartPendingSchema>;

export const oauthFlowStartAuthenticatedSchema = z.object({
  flow_id: z.string().min(1),
  provider: z.string().min(1),
  status: z.literal('authenticated'),
});
export type OAuthFlowStartAuthenticated = z.infer<typeof oauthFlowStartAuthenticatedSchema>;

export const oauthFlowStartSchema = z.discriminatedUnion('status', [
  oauthFlowStartPendingSchema,
  oauthFlowStartAuthenticatedSchema,
]);
export type OAuthFlowStart = z.infer<typeof oauthFlowStartSchema>;

export const oauthFlowSnapshotSchema = z.object({
  flow_id: z.string().min(1),
  provider: z.string().min(1),
  status: oauthFlowStatusEnum,
  verification_uri: z.string().url(),
  verification_uri_complete: z.string().url(),
  user_code: z.string().min(1),
  expires_in: z.number().int().positive(),
  expires_at: isoDateTimeSchema,
  interval: z.number().int().positive(),
  resolved_at: isoDateTimeSchema.optional(),
  error_message: z.string().optional(),
});
export type OAuthFlowSnapshot = z.infer<typeof oauthFlowSnapshotSchema>;

export const oauthLoginQuerySchema = z.object({
  provider: z.string().min(1).optional(),
});
export type OAuthLoginQuery = z.infer<typeof oauthLoginQuerySchema>;

export const oauthLoginCancelResponseSchema = z.object({
  cancelled: z.boolean(),
  status: oauthFlowStatusEnum,
});
export type OAuthLoginCancelResponse = z.infer<typeof oauthLoginCancelResponseSchema>;

export const oauthLogoutRequestSchema = z.object({
  provider: z.string().min(1).optional(),
});
export type OAuthLogoutRequest = z.infer<typeof oauthLogoutRequestSchema>;

export const oauthLogoutResponseSchema = z.object({
  logged_out: z.literal(true),
  provider: z.string().min(1),
});
export type OAuthLogoutResponse = z.infer<typeof oauthLogoutResponseSchema>;

// ── Managed-account read routes ─────────────────────────────────────────────
// NOTE (recorded drift, neither side chosen): the fork's server answers
// `POST /v1/oauth/logout` with the camelCase `{ "loggedOut": bool }` and no
// `provider` (`route_plugins_oauth.rs`), while `oauthLogoutResponseSchema`
// above and the shipped Web bundle both read `logged_out`. The schema is left
// untouched pending a protocol-compat decision.

/**
 * `GET /v1/oauth/usage` — the managed `/usages` payload proxied through
 * verbatim. Only the unauthenticated / failure markers are the fork's own;
 * the success body is the managed API's and is not enumerated here.
 */
export const oauthUsageResponseSchema = z
  .object({
    authenticated: z.boolean().optional(),
    provider: z.string().optional(),
    error: z.string().optional(),
  })
  .passthrough();
export type OAuthUsageResponse = z.infer<typeof oauthUsageResponseSchema>;

/** `GET /v1/oauth/region` — the install region (`resolveKimiRegion`). */
export const oauthRegionResponseSchema = z.object({
  region: z.enum(['mainland-cn', 'global']),
});
export type OAuthRegionResponse = z.infer<typeof oauthRegionResponseSchema>;

/**
 * `GET /v1/oauth/user` | `/v1/oauth/userinfo` — the managed `/me` payload
 * proxied through verbatim; same marker fields as the usage route.
 */
export const oauthUserInfoResponseSchema = z
  .object({
    authenticated: z.boolean().optional(),
    provider: z.string().optional(),
    error: z.string().optional(),
  })
  .passthrough();
export type OAuthUserInfoResponse = z.infer<typeof oauthUserInfoResponseSchema>;
