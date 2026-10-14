// node --test --test-isolation=none contrib/gemini-bridge/bridge.test.mjs
// Fake credentials and a mock upstream: the tests never touch Google and
// never write a real login.
import { test } from 'node:test';
import assert from 'node:assert';
import { createBridge, quotaHeaders, readCreds } from './bridge.mjs';
import { mkdirSync, writeFileSync, readFileSync, unlink, rmdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const KEY = 'a'.repeat(40);
const MODELS = [{ id: 'gemini-2.5-pro', name: 'Gemini 2.5 Pro', inputTokenLimit: 1048576, outputTokenLimit: 65536 }];

// A stand-in for undici's fetch Response: the bridge only reads status,
// headers, body.json() and a byte reader.
function mockResponse(status, payload, headers = {}) {
  const text = JSON.stringify(payload);
  return { status, headers: Object.entries(headers), body: { json: () => payload, text: () => text } };
}
function mockStream(frames, headers = {}) {
  const chunks = frames.map((frame) => Buffer.from(`${frame}\n\n`));
  let index = 0;
  return { status: 200, headers: Object.entries(headers), body: { getReader: () => ({
    read: async () => index < chunks.length ? { done: false, value: chunks[index++] } : { done: true, value: undefined },
  }) } };
}

async function serve(fetchImpl) {
  const creds = { access: 'access-token', refresh: 'refresh-token', client: { client_id: 'cid', client_secret: 'secret' },
    expires: Math.floor(Date.now() / 1000) + 3600 };
  const server = createBridge({ models: MODELS, key: KEY, getCreds: async () => creds, apiKey: 'public-key',
    userAgent: 'gemini-cli/1.0.0', fetchImpl });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return { origin: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

async function request(origin, path, body, extraHeaders = {}) {
  const response = await fetch(`${origin}${path}`, {
    method: body === null ? 'GET' : 'POST',
    headers: { ...(body === null ? {} : { 'content-type': 'application/json' }), ...extraHeaders,
      ...(body === null && path !== '/healthz' ? { 'x-goog-api-key': KEY } : {}),
      ...(body !== null ? { 'x-goog-api-key': KEY } : {}) },
    body: body === null ? undefined : JSON.stringify(body),
  });
  return { status: response.status, headers: response.headers, text: await response.text() };
}

function withTempDir(body) {
  const dir = join(tmpdir(), `arouter-gemini-bridge-${process.pid}-${Math.floor(Math.random() * 1e6)}`);
  mkdirSync(dir, { mode: 0o700 });
  try {
    body(dir);
  } finally {
    for (const name of ['oauth_creds.json', 'flat.json']) {
      try {
        unlink(join(dir, name));
      } catch {
      }
    }
    try {
      rmdirSync(dir);
    } catch {
    }
  }
}

test('a credential file without tokens is not a login', () => {
  try {
    readCreds('/definitely-not-here/oauth_creds.json');
    assert.fail('a credential file with no tokens must not pass');
  } catch (error) {
    assert.equal(error.status, 503, 'a missing login is an unconfigured upstream, not a 400');
  }
});

test('the CLI envelope and the google-auth-library shape both parse', () => {
  withTempDir((dir) => {
    writeFileSync(join(dir, 'oauth_creds.json'), JSON.stringify({ installed: { client_id: 'cid', client_secret: 'secret' },
      auth: { access_token: 'a', refresh_token: 'r', expires_at: 2_000_000_000 } }));
    const envelope = readCreds(join(dir, 'oauth_creds.json'));
    assert.equal(envelope.access, 'a');
    assert.equal(envelope.client.client_id, 'cid');
    writeFileSync(join(dir, 'flat.json'), JSON.stringify({ access_token: 'b', refresh_token: 'q',
      expiry_datetime: 1_900_000_000, client_id: 'flat-cid' }));
    const flat = readCreds(join(dir, 'flat.json'));
    assert.equal(flat.access, 'b');
    assert.equal(flat.expires, 1_900_000_000, 'expiry_datetime is the library spelling');
  });
});

test('healthz is public; inference requires the bridge key', async () => {
  const { origin, close } = await serve((url, options) => mockResponse(200, { candidates: [] }));
  try {
    const health = await fetch(`${origin}/healthz`);
    assert.equal(health.status, 200);
    const wrong = await fetch(`${origin}/v1beta/models`, { headers: { 'x-goog-api-key': 'b'.repeat(40) } });
    assert.equal(wrong.status, 401);
  } finally {
    close();
  }
});

test('discovery sees the Google listing shape', async () => {
  const { origin, close } = await serve((url, options) => mockResponse(200, {}));
  try {
    const listing = await request(origin, '/v1beta/models', null);
    assert.equal(listing.status, 200);
    const models = JSON.parse(listing.text).models;
    assert.equal(models[0].name, 'models/gemini-2.5-pro');
    assert.deepEqual(models[0].supportedGenerationMethods, ['generateContent', 'streamGenerateContent']);
    assert.equal(models[0].inputTokenLimit, 1048576);
  } finally {
    close();
  }
});

test('generateContent is forwarded verbatim', async () => {
  const upstream = { candidates: [{ content: { parts: [{ text: 'hi' }] }, finishReason: 'STOP' }],
    usageMetadata: { promptTokenCount: 3, candidatesTokenCount: 2, totalTokenCount: 5 } };
  const { origin, close } = await serve((url, options) => mockResponse(200, upstream));
  try {
    const response = await request(origin, '/v1beta/models/gemini-2.5-pro:generateContent',
      { contents: [{ role: 'user', parts: [{ text: 'hi' }] }] });
    assert.equal(response.status, 200);
    assert.deepEqual(JSON.parse(response.text), upstream);
  } finally {
    close();
  }
});

test('free-tier quota windows become Retry-After', async () => {
  const rate = { error: { code: 429, message: 'quota', details: [{ retry: { after: 41.7 } }] } };
  const { origin, close } = await serve((url, options) => mockResponse(429, rate, { 'x-goog-v1beta-ratelimit-remaining': '0' }));
  try {
    const response = await request(origin, '/v1beta/models/gemini-2.5-pro:generateContent', { contents: [] });
    assert.equal(response.status, 429);
    assert.equal(response.headers.get('retry-after'), '42', 'arouter harvests Retry-After');
    assert.equal(response.headers.get('x-goog-v1beta-ratelimit-remaining'), null, 'unknown headers are dropped');
  } finally {
    close();
  }
});

test('stream frames are re-emitted without reshaping', async () => {
  const frames = ['data: {"candidates":[{"content":{"parts":[{"text":"he"}]}}]}',
    'data: {"candidates":[{"content":{"parts":[{"text":"llo"}]},"finishReason":"STOP"}]}'];
  let upstreamUrl;
  const { origin, close } = await serve((url, options) => {
    upstreamUrl = url;
    return mockStream(frames);
  });
  try {
    const response = await request(origin, '/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse', { contents: [] });
    assert.equal(response.status, 200);
    assert.match(upstreamUrl, /:streamGenerateContent\?key=public-key&alt=sse$/);
    assert.match(response.text, /finishReason/, 'the terminal event must survive');
  } finally {
    close();
  }
});

test('a model outside the allowlist never reaches upstream', async () => {
  let reached = false;
  const { origin, close } = await serve((url, options) => {
    reached = true;
    return mockResponse(200, {});
  });
  try {
    const response = await request(origin, '/v1beta/models/gemini-9-ultra:generateContent', { contents: [] });
    assert.equal(response.status, 400);
    assert.equal(reached, false, 'the allowlist is enforced before any upstream call');
  } finally {
    close();
  }
});

test('OpenAI fields are rejected, not guessed', async () => {
  const { origin, close } = await serve((url, options) => mockResponse(200, {}));
  try {
    const response = await request(origin, '/v1beta/models/gemini-2.5-pro:generateContent',
      { messages: [{ role: 'user', content: 'hi' }] });
    assert.equal(response.status, 400);
    assert.match(response.text, /messages/, 'the error names the offending field');
  } finally {
    close();
  }
});

test('quota normalization keeps only what arouter can learn', () => {
  const headers = quotaHeaders([['Retry-After', '7'], ['x-ratelimit-remaining', '2'], ['x-goog-date', 'x']], null);
  assert.equal(headers['retry-after'], '7');
  assert.equal(headers['x-ratelimit-remaining'], '2');
  assert.equal(headers['x-goog-date'], undefined);
});
