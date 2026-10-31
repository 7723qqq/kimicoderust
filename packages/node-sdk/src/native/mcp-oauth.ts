/**
 * Host-side MCP OAuth 2.1 authorization-code + PKCE flow.
 *
 * The engine owns credential storage (encrypted at rest) and refresh; the host
 * owns the browser round trip, exactly where v2 puts the boundary
 * (`agent-core-v2/src/mcpCore/oauth/service.ts`): discovery, dynamic client
 * registration, a loopback callback listener, and the code exchange. Tokens
 * are handed back through `persist`, which the native client wires to the
 * engine's store so both sides see one encrypted record.
 *
 * No dependency on the MCP SDK: discovery follows RFC 8414 / RFC 9728,
 * registration RFC 7591, resource indicators RFC 8707, and PKCE S256.
 */
import { createServer, type Server } from 'node:http';
import { createHash, randomBytes, randomUUID } from 'node:crypto';

import { ErrorCodes, KimiError } from '#/error-protocol';

export interface McpOAuthTokenRecord {
  access_token: string;
  refresh_token?: string;
  expires_at_ms?: number;
  token_endpoint: string;
  client_id: string;
  client_secret?: string;
  resource?: string;
  obtained_at_ms?: number;
}

export interface McpOAuthBeginInput {
  readonly name: string;
  readonly serverUrl: string;
  readonly clientName?: string;
  /** Persist the exchanged grant (the native client forwards it to the engine). */
  readonly persist: (tokens: McpOAuthTokenRecord) => Promise<void> | void;
}

export interface McpOAuthBeginResult {
  readonly flowId: string;
  readonly authorizationUrl: string;
}

interface AuthorizationServerMetadata {
  readonly authorizationEndpoint: string;
  readonly tokenEndpoint: string;
  readonly registrationEndpoint?: string;
}

interface PendingFlow {
  readonly name: string;
  readonly serverUrl: string;
  readonly resource: string;
  readonly callback: CallbackServer;
  readonly tokenEndpoint: string;
  readonly clientId: string;
  readonly clientSecret?: string;
  readonly codeVerifier: string;
  readonly state: string;
  readonly persist: McpOAuthBeginInput['persist'];
}

interface CallbackServer {
  readonly redirectUri: string;
  waitForCode(options?: { readonly timeoutMs?: number; readonly signal?: AbortSignal }): Promise<{
    code: string;
    state: string;
  }>;
  close(): Promise<void>;
}

const flows = new Map<string, PendingFlow>();

const DISCOVERY_TIMEOUT_MS = 10_000;
const DEFAULT_AUTH_TIMEOUT_MS = 5 * 60_000;

function oauthError(message: string, cause?: unknown): KimiError {
  return new KimiError(ErrorCodes.MCP_OAUTH_FAILED, message, { cause });
}

function trimSlashes(value: string): string {
  return value.replace(/\/+$/, '');
}

async function fetchJson(url: string): Promise<Record<string, unknown> | undefined> {
  try {
    const response = await fetch(url, {
      headers: { accept: 'application/json' },
      signal: AbortSignal.timeout(DISCOVERY_TIMEOUT_MS),
    });
    if (!response.ok) return undefined;
    const body: unknown = await response.json();
    return typeof body === 'object' && body !== null && !Array.isArray(body)
      ? (body as Record<string, unknown>)
      : undefined;
  } catch {
    return undefined;
  }
}

function asString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

/**
 * RFC 9728 protected-resource metadata (optional), then RFC 8414 / OIDC
 * authorization-server metadata. The path-aware well-known variant is tried
 * before the origin-level one, so an AS mounted under a path is found.
 */
async function discoverAuthorizationServer(
  serverUrl: string,
): Promise<{ metadata: AuthorizationServerMetadata; resource: string; scope?: string }> {
  const resourceUrl = new URL(serverUrl);
  const resource = `${resourceUrl.origin}${trimSlashes(resourceUrl.pathname)}`.replace(
    /\/+$/,
    '',
  );

  let protectedResource: Record<string, unknown> | undefined;
  for (const candidate of [
    `${resourceUrl.origin}/.well-known/oauth-protected-resource${trimSlashes(resourceUrl.pathname)}`,
    `${resourceUrl.origin}/.well-known/oauth-protected-resource`,
  ]) {
    protectedResource = await fetchJson(candidate);
    if (protectedResource !== undefined) break;
  }

  const issuers: string[] = [];
  const advertised = protectedResource?.['authorization_servers'];
  if (Array.isArray(advertised)) {
    for (const issuer of advertised) {
      const value = asString(issuer);
      if (value !== undefined) issuers.push(value);
    }
  }
  issuers.push(resourceUrl.origin);

  const scope =
    typeof protectedResource?.['scope'] === 'string'
      ? protectedResource['scope']
      : undefined;

  for (const issuer of new Set(issuers)) {
    let base: URL;
    try {
      base = new URL(issuer);
    } catch {
      continue;
    }
    const prefix = trimSlashes(base.pathname);
    const candidates = [
      `${base.origin}/.well-known/oauth-authorization-server${prefix}`,
      `${base.origin}/.well-known/oauth-authorization-server`,
      `${base.origin}/.well-known/openid-configuration${prefix}`,
      `${base.origin}/.well-known/openid-configuration`,
    ];
    for (const candidate of new Set(candidates)) {
      const metadata = await fetchJson(candidate);
      const authorizationEndpoint = asString(metadata?.['authorization_endpoint']);
      const tokenEndpoint = asString(metadata?.['token_endpoint']);
      if (authorizationEndpoint !== undefined && tokenEndpoint !== undefined) {
        return {
          metadata: {
            authorizationEndpoint,
            tokenEndpoint,
            registrationEndpoint: asString(metadata?.['registration_endpoint']),
          },
          resource,
          scope,
        };
      }
    }
  }
  throw oauthError(`MCP OAuth discovery failed for ${serverUrl}`);
}

/** RFC 7591 dynamic client registration for a public PKCE client. */
async function registerClient(
  registrationEndpoint: string,
  redirectUri: string,
  clientName: string,
): Promise<{ clientId: string; clientSecret?: string }> {
  let response: Response;
  try {
    response = await fetch(registrationEndpoint, {
      method: 'POST',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify({
        client_name: clientName,
        redirect_uris: [redirectUri],
        grant_types: ['authorization_code', 'refresh_token'],
        response_types: ['code'],
        token_endpoint_auth_method: 'none',
      }),
      signal: AbortSignal.timeout(DISCOVERY_TIMEOUT_MS),
    });
  } catch (error) {
    throw oauthError('MCP OAuth client registration failed', error);
  }
  if (!response.ok) {
    throw oauthError(`MCP OAuth client registration failed (HTTP ${String(response.status)})`);
  }
  const body = (await response.json()) as Record<string, unknown>;
  const clientId = asString(body['client_id']);
  if (clientId === undefined) {
    throw oauthError('MCP OAuth registration returned no client_id');
  }
  return { clientId, clientSecret: asString(body['client_secret']) };
}

function base64Url(buffer: Buffer): string {
  return buffer.toString('base64').replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '');
}

function htmlPage(title: string, message: string): string {
  return `<!doctype html><html><head><meta charset="utf-8"><title>${title}</title></head>
<body style="font-family:system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem">
<h1>${title}</h1><p>${message}</p></body></html>`;
}

function startCallbackServer(): Promise<CallbackServer> {
  return new Promise((resolve, reject) => {
    let settle:
      | { resolve: (value: { code: string; state: string }) => void; reject: (error: Error) => void }
      | undefined;
    let received: { code: string; state: string } | undefined;
    let closed = false;

    const server: Server = createServer((request, response) => {
      const url = new URL(request.url ?? '/', 'http://127.0.0.1');
      if (url.pathname !== '/callback') {
        response.writeHead(404, { 'content-type': 'text/plain' });
        response.end('not found');
        return;
      }
      const error = url.searchParams.get('error');
      const code = url.searchParams.get('code');
      const state = url.searchParams.get('state') ?? '';
      if (error !== null) {
        response.writeHead(400, { 'content-type': 'text/html' });
        response.end(htmlPage('Authorization failed', `The provider returned: ${error}`));
        settle?.reject(oauthError(`MCP OAuth provider returned "${error}"`));
        settle = undefined;
        return;
      }
      if (code === null || code.length === 0) {
        response.writeHead(400, { 'content-type': 'text/html' });
        response.end(htmlPage('Authorization failed', 'The callback carried no code.'));
        settle?.reject(oauthError('MCP OAuth callback carried no authorization code'));
        settle = undefined;
        return;
      }
      response.writeHead(200, { 'content-type': 'text/html' });
      response.end(
        htmlPage('Authorization complete', 'You can close this window and return to Kimi Code.'),
      );
      received = { code, state };
      settle?.resolve(received);
      settle = undefined;
    });

    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      if (address === null || typeof address === 'string') {
        server.close();
        reject(oauthError('MCP OAuth callback listener has no address'));
        return;
      }
      resolve({
        redirectUri: `http://127.0.0.1:${String(address.port)}/callback`,
        waitForCode(options = {}) {
          if (received !== undefined) return Promise.resolve(received);
          if (closed) return Promise.reject(oauthError('MCP OAuth callback listener closed'));
          const timeoutMs = options.timeoutMs ?? DEFAULT_AUTH_TIMEOUT_MS;
          return new Promise((resolveCode, rejectCode) => {
            const onAbort = (): void => {
              settle = undefined;
              clearTimeout(timer);
              rejectCode(oauthError('MCP OAuth authorization was cancelled'));
            };
            const timer = setTimeout(() => {
              settle = undefined;
              options.signal?.removeEventListener('abort', onAbort);
              rejectCode(oauthError('MCP OAuth callback timed out'));
            }, timeoutMs);
            timer.unref?.();
            options.signal?.addEventListener('abort', onAbort, { once: true });
            settle = {
              resolve: (value) => {
                clearTimeout(timer);
                options.signal?.removeEventListener('abort', onAbort);
                resolveCode(value);
              },
              reject: (error) => {
                clearTimeout(timer);
                options.signal?.removeEventListener('abort', onAbort);
                rejectCode(error);
              },
            };
          });
        },
        close() {
          if (closed) return Promise.resolve();
          closed = true;
          return new Promise((resolveClose) => {
            server.close(() => resolveClose());
          });
        },
      });
    });
  });
}

export async function beginMcpOAuthFlow(input: McpOAuthBeginInput): Promise<McpOAuthBeginResult> {
  // Validate the URL before allocating any listener.
  const serverUrl = new URL(input.serverUrl).toString();
  const callback = await startCallbackServer();
  try {
    const { metadata, resource, scope } = await discoverAuthorizationServer(serverUrl);
    const registration =
      metadata.registrationEndpoint === undefined
        ? undefined
        : await registerClient(
            metadata.registrationEndpoint,
            callback.redirectUri,
            input.clientName ?? 'Kimi Code',
          );
    if (registration === undefined) {
      throw oauthError(
        'MCP OAuth authorization server offers no dynamic client registration',
      );
    }

    const codeVerifier = base64Url(randomBytes(64));
    const codeChallenge = base64Url(createHash('sha256').update(codeVerifier).digest());
    const state = randomUUID();
    const authorizationUrl = new URL(metadata.authorizationEndpoint);
    authorizationUrl.searchParams.set('response_type', 'code');
    authorizationUrl.searchParams.set('client_id', registration.clientId);
    authorizationUrl.searchParams.set('redirect_uri', callback.redirectUri);
    authorizationUrl.searchParams.set('code_challenge', codeChallenge);
    authorizationUrl.searchParams.set('code_challenge_method', 'S256');
    authorizationUrl.searchParams.set('state', state);
    authorizationUrl.searchParams.set('resource', resource);
    if (scope !== undefined) authorizationUrl.searchParams.set('scope', scope);

    const flowId = randomUUID();
    flows.set(flowId, {
      name: input.name,
      serverUrl,
      resource,
      callback,
      tokenEndpoint: metadata.tokenEndpoint,
      clientId: registration.clientId,
      clientSecret: registration.clientSecret,
      codeVerifier,
      state,
      persist: input.persist,
    });
    return { flowId, authorizationUrl: authorizationUrl.toString() };
  } catch (error) {
    await callback.close();
    throw error;
  }
}

/** Wait for the browser callback, exchange the code, and persist the grant. */
export async function completeMcpOAuthFlow(
  flowId: string,
  options: { readonly timeoutMs?: number; readonly signal?: AbortSignal } = {},
): Promise<{ name: string }> {
  const flow = flows.get(flowId);
  if (flow === undefined) {
    throw oauthError(`Unknown or expired MCP OAuth flow: ${flowId}`);
  }
  try {
    const { code, state } = await flow.callback.waitForCode(options);
    if (state !== flow.state) {
      throw oauthError('MCP OAuth state mismatch — possible CSRF; refusing token exchange');
    }
    const form = new URLSearchParams({
      grant_type: 'authorization_code',
      code,
      redirect_uri: flow.callback.redirectUri,
      client_id: flow.clientId,
      code_verifier: flow.codeVerifier,
      resource: flow.resource,
    });
    if (flow.clientSecret !== undefined) form.set('client_secret', flow.clientSecret);
    let response: Response;
    try {
      response = await fetch(flow.tokenEndpoint, {
        method: 'POST',
        headers: {
          'content-type': 'application/x-www-form-urlencoded',
          accept: 'application/json',
        },
        body: form.toString(),
        signal: options.signal,
      });
    } catch (error) {
      throw oauthError('MCP OAuth token exchange failed', error);
    }
    if (!response.ok) {
      const detail = await response.text().catch(() => '');
      throw oauthError(
        `MCP OAuth token exchange failed (HTTP ${String(response.status)})${detail ? `: ${detail.slice(0, 200)}` : ''}`,
      );
    }
    const body = (await response.json()) as Record<string, unknown>;
    const accessToken = asString(body['access_token']);
    if (accessToken === undefined) {
      throw oauthError('MCP OAuth token response carried no access_token');
    }
    const expiresIn = typeof body['expires_in'] === 'number' ? body['expires_in'] : undefined;
    await flow.persist({
      access_token: accessToken,
      refresh_token: asString(body['refresh_token']),
      expires_at_ms: expiresIn === undefined ? undefined : Date.now() + expiresIn * 1000,
      token_endpoint: flow.tokenEndpoint,
      client_id: flow.clientId,
      client_secret: flow.clientSecret,
      resource: flow.resource,
      obtained_at_ms: Date.now(),
    });
    return { name: flow.name };
  } finally {
    flows.delete(flowId);
    await flow.callback.close();
  }
}

export async function cancelMcpOAuthFlow(flowId: string): Promise<void> {
  const flow = flows.get(flowId);
  if (flow === undefined) return;
  flows.delete(flowId);
  await flow.callback.close();
}
