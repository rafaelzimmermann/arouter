#!/usr/bin/env node
// Optional local Google generateContent adapter for an existing Gemini CLI
// login. Unlike the Codex bridge this one does not translate: arouter's
// `wire = "gemini"` adapter already speaks Google's shape, so the bridge is
// an auth shim — it reads Gemini CLI's stored OAuth credential on every
// request and answers Google's own request/response format verbatim.
//
//   GET  /healthz
//   GET  /v1beta/models
//   POST /v1beta/models/{name}:generateContent
//   POST /v1beta/models/{name}:streamGenerateContent?alt=sse
//
// Gemini owns login and token renewal: this helper never starts an
// interactive OAuth flow, and it refuses to serve when the login is gone.
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { readFile, writeFile, rename, mkdir } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { timingSafeEqual } from 'node:crypto';
import { once } from 'node:events';

function invalid(message, status = 400) {
  return Object.assign(new Error(message), { status });
}

// ───────────────── credential: Gemini CLI's own file ─────────────────

// Gemini CLI (and google-auth-library) write the credential under ~/.gemini.
// Two shapes appear in the wild: a flat google-auth-library object, and the
// CLI's { installed, auth } envelope. Accept both, and never invent a login.
export function readCreds(path) {
  let raw;
  try {
    raw = JSON.parse(readFileSync(path));
  } catch {
    throw invalid(`Gemini login absent at ${path}: sign in with Gemini CLI first`, 503);
  }
  const envelope = raw.auth ?? raw;
  const client = raw.installed ?? raw.third_party ?? raw;
  if (!envelope.access_token || !envelope.refresh_token) {
    throw invalid('Gemini login has no access/refresh token; sign in with Gemini CLI', 503);
  }
  // google-auth-library writes expiry_datetime, the CLI writes expires_at —
  // both in epoch seconds. A credential without either is treated as stale.
  const expires = envelope.expires_at ?? envelope.expiry_datetime ?? envelope.expires ?? 0;
  return { access: envelope.access_token, refresh: envelope.refresh_token, client, expires: Number(expires) };
}

// Access tokens expire hourly; refreshing is the only credential work this
// helper does. When Google rotates the refresh token it is written back to
// the same file the CLI reads, atomically and mode 0600.
export function refreshCreds(path, creds, tokenUrl, fetchImpl = globalThis.fetch) {
  const client = creds.client;
  if (!client.client_id) throw invalid('Gemini login has no OAuth client id; refresh is impossible', 503);
  const form = new URLSearchParams({ client_id: client.client_id, grant_type: 'refresh_token',
    refresh_token: creds.refresh });
  if (client.client_secret) form.set('client_secret', client.client_secret);
  return fetchImpl(tokenUrl, { method: 'POST', body: form.toString(),
    headers: { 'content-type': 'application/x-www-form-urlencoded' } });
}

export function persistCreds(path, creds, merged) {
  const onDisk = JSON.parse(readFileSync(path));
  const next = { ...onDisk, ...(onDisk.auth ? { auth: { ...onDisk.auth, access_token: merged.access_token,
    expires_at: Math.floor(Date.now() / 1000) + Number(merged.expires_in ?? 3600) } } : {}),
    refresh_token: merged.refresh_token ?? creds.refresh };
  const dir = path.slice(0, path.lastIndexOf('/')) || '.';
  mkdir(dir, { mode: 0o700 });
  const temp = `${path}.tmp-${process.pid}`;
  writeFile(temp, `${JSON.stringify(next, null, 2)}\n`, { mode: 0o600 });
  rename(temp, path);
}

// ───────────────── quota: what arouter can actually learn ─────────────────

// Google reports quota pressure as a 429 with retryAfterSeconds buried in the
// error body (the free tier) or a bare Retry-After header. arouter's quota
// watcher reads Retry-After and x-ratelimit-*, so normalize both into headers.
export function quotaHeaders(headers, body) {
  const out = {};
  for (const [name, value] of headers ?? []) {
    const lower = name.toLowerCase();
    if (lower === 'retry-after' || lower.startsWith('x-ratelimit-')) out[lower] = String(value);
  }
  const details = body?.error?.details ?? [];
  const seconds = details.find((d) => d?.retry?.after)?.retry.after ?? body?.error?.retry_after?.seconds;
  if (seconds && !out['retry-after']) out['retry-after'] = String(Math.ceil(Number(seconds)));
  return out;
}

// ───────────────── the bridge's own local key ─────────────────

// arouter's gemini wire carries the credential in x-goog-api-key (Google's
// JSON-API header), so that is the header the bridge key must appear in.
function authorized(headers, key) {
  if (!key || key.length < 32) return false;
  const given = headers['x-goog-api-key'] ?? /^Bearer (.+)$/.exec(headers.authorization ?? '');
  const actual = Buffer.from(String(given ?? ''));
  const expected = Buffer.from(key);
  return actual.length === expected.length && timingSafeEqual(actual, expected);
}

function json(res, status, payload, headers = {}) {
  res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-cache', ...headers });
  res.end(`${JSON.stringify(payload)}\n`);
}

async function readBody(req) {
  const chunks = [];
  let size = 0;
  await new Promise((done, failed) => {
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > 32 * 1024 * 1024) return failed(invalid('Request body too large', 413));
      chunks.push(chunk);
    });
    req.on('end', done);
    req.on('error', failed);
  });
  const raw = Buffer.concat(chunks).toString();
  if (!raw) return null;
  try {
    return JSON.parse(raw);
  } catch {
    throw invalid('Expected a JSON body', 400);
  }
}

// ───────────────── the bridge ─────────────────

export function createBridge({ models, key, getCreds, upstream = 'https://generativelanguage.googleapis.com/v1beta',
  apiKey, userAgent, fetchImpl = globalThis.fetch, timeoutMs = 180_000 }) {
  if (!key || key.length < 32) throw new Error('Bridge key must have at least 32 characters');
  return createServer(async (req, res) => {
    const abort = new AbortController();
    const onClose = () => abort.abort();
    res.on('close', onClose);
    const timer = setTimeout(() => abort.abort(), timeoutMs);
    timer.unref();
    let responseHeaders = {};
    try {
      if (req.url === '/healthz' && req.method === 'GET') {
        await getCreds();
        return json(res, 200, { status: 'ok', provider: 'gemini-subscription' });
      }
      if (!authorized(req.headers, key)) throw invalid('Unauthorized', 401);
      if (req.url === '/v1beta/models' && req.method === 'GET') {
        // Google's listing shape — arouter's discovery reads name,
        // supportedGenerationMethods and inputTokenLimit off exactly this.
        return json(res, 200, { models: models.map((m) => ({ name: `models/${m.id}`, displayName: m.name,
          supportedGenerationMethods: ['generateContent', 'streamGenerateContent'], inputTokenLimit: m.inputTokenLimit,
          outputTokenLimit: m.outputTokenLimit })) });
      }
      const requestUrl = new URL(req.url, 'http://bridge');
      const path = requestUrl.pathname.startsWith('/v1beta/models/')
        ? requestUrl.pathname.slice('/v1beta/models/'.length) : null;
      if (!path) throw invalid('Not found', 404);
      const [head, verb] = path.split(':');
      const id = head?.startsWith('models/') ? head.slice('models/'.length) : head;
      if (!id) throw invalid('Not found', 404);
      if (verb !== 'generateContent' && verb !== 'streamGenerateContent') throw invalid('Not found', 404);
      if (requestUrl.searchParams.has('alt') && requestUrl.searchParams.get('alt') !== 'sse') {
        throw invalid('Only alt=sse is supported', 400);
      }
      if (req.method !== 'POST') throw invalid('generateContent requires POST', 405);
      const model = models.find((m) => m.id === id);
      if (!model) throw invalid('Model is not enabled on this bridge');
      const body = await readBody(req);
      if (!body || typeof body !== 'object' || Array.isArray(body)) throw invalid('Expected a JSON object');
      // The router already translated the request; only Google's own keys are
      // accepted, so a stray OpenAI field cannot silently mean something else.
      for (const stray of ['messages', 'prompt', 'model', 'stream']) {
        if (body[stray] != null) throw invalid(`${stray} is not a generateContent field`, 400);
      }
      const creds = await getCreds();
      const query = new URLSearchParams();
      if (apiKey) query.set('key', apiKey);
      if (verb === 'streamGenerateContent') query.set('alt', 'sse');
      const headers = { 'content-type': 'application/json', authorization: `Bearer ${creds.access}`,
        'user-agent': userAgent, 'x-goog-api-client': userAgent, 'x-clicommand': 'prompt' };
      const suffix = query.size ? `?${query.toString()}` : '';
      const upstreamResponse = await fetchImpl(`${upstream}/models/${id}:${verb}${suffix}`, {
        method: 'POST', headers, body: JSON.stringify(body), signal: abort.signal });
      if (verb === 'generateContent') {
        const payload = await upstreamResponse.body.json();
        responseHeaders = quotaHeaders(upstreamResponse.headers, payload);
        if (upstreamResponse.status !== 200) throw invalid(JSON.stringify(payload) || 'Gemini request failed',
          upstreamResponse.status);
        return json(res, 200, payload, responseHeaders);
      }
      // Google's SSE is re-emitted byte for byte: arouter's GeminiSse
      // translator owns the OpenAI side, so nothing here reshapes the wire.
      if (upstreamResponse.status !== 200) {
        const payload = await upstreamResponse.body.json();
        responseHeaders = quotaHeaders(upstreamResponse.headers, payload);
        throw invalid(JSON.stringify(payload) || 'Gemini stream failed', upstreamResponse.status);
      }
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache',
        ...quotaHeaders(upstreamResponse.headers) });
      const reader = upstreamResponse.body.getReader();
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        if (res.destroyed) throw invalid('Client disconnected', 499);
        if (!res.write(value)) await once(res, 'drain', { signal: abort.signal });
      }
      res.end();
    } catch (error) {
      abort.abort();
      if (res.destroyed) return;
      const status = error.status ?? 502;
      const message = error.status ? error.message : 'Gemini bridge request failed';
      if (res.headersSent) res.destroy();
      else json(res, status, { error: { code: status, message, status } }, responseHeaders);
    } finally {
      clearTimeout(timer);
      res.off('close', onClose);
    }
  });
}

async function main() {
  const authPath = process.env.GEMINI_AUTH_FILE ?? join(homedir(), '.gemini', 'oauth_creds.json');
  const configDir = join(homedir(), '.config/arouter');
  const key = (await readFile(process.env.AROUTER_GEMINI_KEY_FILE ?? join(configDir, 'gemini-bridge.key'), 'utf8')).trim();
  const requested = (process.env.AROUTER_GEMINI_MODELS ?? 'gemini-2.5-pro,gemini-2.5-flash').split(',')
    .map((s) => s.trim()).filter(Boolean);
  const models = requested.map((id) => ({ id, name: id, inputTokenLimit: 1_048_576, outputTokenLimit: 65_536 }));
  // The CLI's public key and user agent are not ours to hardcode: they rotate
  // with the CLI. Read them from the installed package, or set the env pair.
  const userAgent = process.env.GEMINI_CLI_USER_AGENT ?? 'gemini-cli/unknown';
  const apiKey = process.env.GEMINI_CLI_API_KEY;
  const tokenUrl = process.env.GEMINI_TOKEN_URL ?? 'https://oauth2.googleapis.com/token';
  const getCreds = async () => {
    const creds = readCreds(authPath);
    if (creds.expires > Date.now() / 1000 + 60) return creds;
    const response = await refreshCreds(authPath, creds, tokenUrl);
    if (response.status !== 200) throw invalid('Gemini login could not be refreshed', 503);
    const merged = await response.body.json();
    persistCreds(authPath, creds, merged);
    return { ...creds, access: merged.access_token };
  };
  await getCreds();
  const server = createBridge({ models, key, getCreds, apiKey, userAgent,
    upstream: process.env.GEMINI_UPSTREAM ?? 'https://generativelanguage.googleapis.com/v1beta' });
  const port = Number(process.env.AROUTER_GEMINI_PORT ?? 3940);
  const bindHost = process.env.AROUTER_GEMINI_HOST ?? '127.0.0.1';
  server.listen(port, bindHost, () => console.log(`Gemini subscription bridge: ${bindHost}:${port}; models=${requested.join(',')}`));
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => {
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 5000).unref();
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(() => { console.error('Gemini bridge startup failed: check Gemini CLI login and bridge key'); process.exitCode = 1; });
}
