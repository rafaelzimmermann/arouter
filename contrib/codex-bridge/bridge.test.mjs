import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { once } from 'node:events';
import { createBridge, toContext, usageFor, readCodexToken, quotaHeaders } from './bridge.mjs';

const key = 'test-only-local-bridge-key-0123456789';
const model = { id: 'test-codex', api: 'openai-codex-responses', provider: 'openai-codex', contextWindow: 1000 };
const message = { content: [{ type: 'text', text: 'OK' }], stopReason: 'stop',
  usage: { input: 3, cacheRead: 5, cacheWrite: 0, output: 2 } };
const body = { model: model.id, messages: [{ role: 'user', content: 'hello' }] };

async function fixture(t, stream, extra = {}) {
  const server = createBridge({ models: [model], key, getToken: async () => 'test-token', stream,
    normalizeContext: x => x, ...extra });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  const root = `http://127.0.0.1:${server.address().port}`;
  const post = (data = body) => fetch(`${root}/v1/chat/completions`, { method: 'POST',
    headers: { authorization: `Bearer ${key}`, 'content-type': 'application/json' }, body: JSON.stringify(data) });
  return { root, post };
}

test('tool history retains call IDs, roles, arguments and definitions', () => {
  const context = toContext({ messages: [
    { role: 'developer', content: 'Be helpful' },
    { role: 'assistant', content: null, tool_calls: [{ id: 'c1', type: 'function', function: { name: 'weather', arguments: '{"city":"Madrid"}' } }] },
    { role: 'tool', tool_call_id: 'c1', content: 'sunny' },
  ], tools: [{ type: 'function', function: { name: 'weather', parameters: { type: 'object' } } }] }, model);
  assert.equal(context.messages[0].role, 'system');
  assert.deepEqual(context.messages[1].content[0].arguments, { city: 'Madrid' });
  assert.equal(context.messages[2].role, 'toolResult');
  assert.equal(context.messages[2].toolCallId, 'c1');
  assert.equal(context.messages[2].toolName, 'weather');
  assert.equal(context.tools[0].name, 'weather');
});

test('rejects malformed tool arguments, orphan results and remote image URLs', () => {
  assert.throws(() => toContext({ messages: [{ role: 'assistant', tool_calls: [
    { type: 'function', id: 'c1', function: { name: 'x', arguments: '{' } },
  ] }] }, model), /valid JSON/);
  assert.throws(() => toContext({ messages: [{ role: 'tool', tool_call_id: 'unknown', content: '' }] }, model), /preceding/);
  assert.throws(() => toContext({ messages: [{ role: 'user', content: [{ type: 'image_url', image_url: { url: 'http://example.test/x' } }] }] }, model), /base64/);
});

test('usage includes cached input tokens', () => {
  assert.deepEqual(usageFor(message), { prompt_tokens: 8, completion_tokens: 2, total_tokens: 10,
    prompt_tokens_details: { cached_tokens: 5 } });
});

test('maps both subscription quota windows and Retry-After', () => {
  assert.deepEqual(quotaHeaders({ 'x-codex-primary-used-percent': '80', 'x-codex-primary-reset-after-seconds': '20',
    'x-codex-secondary-used-percent': '100', 'x-codex-secondary-reset-after-seconds': '50', 'retry-after': '50' }, 1000), {
    'retry-after': '50', 'x-ratelimit-requests-limit': '100', 'x-ratelimit-requests-remaining': '20',
    'x-ratelimit-reset-requests': '1020', 'x-ratelimit-tokens-limit': '100', 'x-ratelimit-tokens-remaining': '0',
    'x-ratelimit-reset-tokens': '1050',
  });
});

test('token reader requires subscription login, detects expiry and sees rotation', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'arouter-codex-test-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const path = join(dir, 'auth.json');
  const token = exp => `header.${Buffer.from(JSON.stringify({ exp })).toString('base64url')}.signature`;
  await writeFile(path, JSON.stringify({ auth_mode: 'apikey', OPENAI_API_KEY: 'test' }));
  await assert.rejects(readCodexToken(path), /subscription/);
  await writeFile(path, JSON.stringify({ auth_mode: 'chatgpt', tokens: { access_token: token(1) } }));
  await assert.rejects(readCodexToken(path), /expired/);
  const fresh = token(Math.floor(Date.now() / 1000) + 3600);
  await writeFile(path, JSON.stringify({ auth_mode: 'chatgpt', tokens: { access_token: fresh } }));
  assert.equal(await readCodexToken(path), fresh);
});

test('discovery requires bridge auth and lists only allowed models', async t => {
  const { root } = await fixture(t, async function* () { throw new Error('must not call upstream'); });
  assert.equal((await fetch(`${root}/v1/models`)).status, 401);
  const response = await fetch(`${root}/v1/models`, { headers: { authorization: `Bearer ${key}` } });
  assert.deepEqual((await response.json()).data.map(m => m.id), [model.id]);
});

test('nonstream completion relays text and usage', async t => {
  const { post } = await fixture(t, async function* (_model, context, options) {
    assert.equal(options.apiKey, 'test-token');
    assert.equal(options.transport, 'sse');
    assert.equal(context.messages[0].content[0].text, 'hello');
    yield { type: 'start', partial: message };
    yield { type: 'done', message };
  });
  const response = await post();
  assert.equal(response.status, 200);
  const result = await response.json();
  assert.equal(result.choices[0].message.content, 'OK');
  assert.equal(result.usage.prompt_tokens, 8);
});

test('stream includes text, tools, final usage and DONE', async t => {
  const call = { id: 'c1', name: 'weather', arguments: { city: 'Madrid' }, type: 'toolCall' };
  const { post } = await fixture(t, async function* () {
    yield { type: 'start' };
    yield { type: 'text_delta', delta: 'OK' };
    yield { type: 'toolcall_end', toolCall: call };
    yield { type: 'done', message: { ...message, stopReason: 'toolUse' } };
  });
  const response = await post({ ...body, stream: true });
  const frames = (await response.text()).split('\n\n').filter(Boolean).map(s => s.slice(6));
  assert.equal(frames.pop(), '[DONE]');
  const chunks = frames.map(s => JSON.parse(s));
  assert.equal(chunks[1].choices[0].delta.content, 'OK');
  assert.equal(chunks[2].choices[0].delta.tool_calls[0].function.arguments, '{"city":"Madrid"}');
  assert.equal(chunks[3].choices[0].finish_reason, 'tool_calls');
  assert.equal(chunks[4].usage.total_tokens, 10);
});

test('pre-stream quota refusal retains HTTP 429 for router failover', async t => {
  const { post } = await fixture(t, async function* (_model, _context, options) {
    await options.fetch('https://example.test', {});
    yield { type: 'error', error: { errorMessage: 'private upstream details' } };
  }, { fetchImpl: async () => new Response('', { status: 429, headers: { 'retry-after': '30' } }) });
  const response = await post({ ...body, stream: true });
  assert.equal(response.status, 429);
  assert.equal(response.headers.get('retry-after'), '30');
  assert.doesNotMatch(await response.text(), /private upstream details/);
});

test('downstream cancellation aborts the upstream stream', async t => {
  let sawAbort;
  const aborted = new Promise(resolve => { sawAbort = resolve; });
  const { post } = await fixture(t, async function* (_model, _context, options) {
    const waitAbort = new Promise(resolve => options.signal.addEventListener('abort', () => { sawAbort(); resolve(); }, { once: true }));
    yield { type: 'start' };
    await waitAbort;
    yield { type: 'error', error: {} };
  });
  const response = await post({ ...body, stream: true });
  await response.body.cancel();
  await Promise.race([aborted, new Promise((_, reject) => {
    const timer = setTimeout(() => reject(new Error('upstream was not aborted')), 1000);
    timer.unref();
  })]);
});

test('deadline aborts a stuck upstream before headers', async t => {
  const { post } = await fixture(t, async function* (_model, _context, options) {
    await new Promise(resolve => options.signal.addEventListener('abort', resolve, { once: true }));
    yield { type: 'error', error: {} };
  }, { timeoutMs: 50 });
  assert.equal((await post()).status, 502);
});
