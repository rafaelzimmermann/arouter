#!/usr/bin/env node
// Optional local Chat Completions adapter for an existing Codex subscription.
// Reuses Pi's protocol adapter and reads Codex's login on every request.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { resolve, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { randomUUID, timingSafeEqual } from 'node:crypto';
import { once } from 'node:events';

function invalid(message, status = 400) {
  return Object.assign(new Error(message), { status });
}

function parts(content) {
  if (content == null) return [];
  if (typeof content === 'string') return [{ type: 'text', text: content }];
  if (!Array.isArray(content)) throw invalid('Message content must be text or an array');
  return content.map(part => {
    if (part.type === 'text' && typeof part.text === 'string') return { type: 'text', text: part.text };
    if (part.type === 'image_url') {
      const match = /^data:(image\/[\w.+-]+);base64,([A-Za-z0-9+/=\s]+)$/.exec(part.image_url?.url ?? '');
      if (!match) throw invalid('Images must use base64 data URLs');
      return { type: 'image', mimeType: match[1], data: match[2] };
    }
    throw invalid(`Unsupported content type: ${part.type}`);
  });
}

const emptyUsage = () => ({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } });

export function toContext(body, model) {
  if (!Array.isArray(body.messages) || !body.messages.length) throw invalid('messages must be a nonempty array');
  const toolNames = new Map();
  const messages = body.messages.map(message => {
    const timestamp = Date.now();
    if (message.role === 'system' || message.role === 'developer') {
      const content = parts(message.content);
      if (content.some(p => p.type !== 'text')) throw invalid('System messages must contain text');
      return { role: 'system', content, timestamp };
    }
    if (message.role === 'user') return { role: 'user', content: parts(message.content), timestamp };
    if (message.role === 'tool') {
      if (!toolNames.has(message.tool_call_id)) throw invalid('Tool result has no preceding tool call');
      return { role: 'toolResult', toolCallId: message.tool_call_id, toolName: toolNames.get(message.tool_call_id),
        content: parts(message.content), isError: false, timestamp };
    }
    if (message.role !== 'assistant') throw invalid(`Unsupported role: ${message.role}`);
    const content = parts(message.content);
    if (message.tool_calls !== undefined && !Array.isArray(message.tool_calls)) throw invalid('tool_calls must be an array');
    for (const call of message.tool_calls ?? []) {
      if (call.type !== 'function' || !call.id || !call.function?.name) throw invalid('Invalid function tool call');
      let args;
      try { args = JSON.parse(call.function.arguments); } catch { throw invalid('Tool arguments must be valid JSON'); }
      if (!args || typeof args !== 'object' || Array.isArray(args)) throw invalid('Tool arguments must be an object');
      toolNames.set(call.id, call.function.name);
      content.push({ type: 'toolCall', id: call.id, name: call.function.name, arguments: args });
    }
    return { role: 'assistant', content, api: model.api, provider: model.provider, model: model.id,
      usage: emptyUsage(), stopReason: message.tool_calls?.length ? 'toolUse' : 'stop', timestamp };
  });
  if (body.tools !== undefined && !Array.isArray(body.tools)) throw invalid('tools must be an array');
  const tools = (body.tools ?? []).map(tool => {
    if (tool.type !== 'function' || !tool.function?.name) throw invalid('Only function tools are supported');
    return { name: tool.function.name, description: tool.function.description ?? '',
      parameters: tool.function.parameters ?? { type: 'object', properties: {} } };
  });
  return { messages, tools };
}

export function usageFor(message) {
  const u = message.usage ?? {};
  const input = (u.input ?? 0) + (u.cacheRead ?? 0) + (u.cacheWrite ?? 0);
  return { prompt_tokens: input, completion_tokens: u.output ?? 0, total_tokens: input + (u.output ?? 0),
    prompt_tokens_details: { cached_tokens: u.cacheRead ?? 0 } };
}

function toolCall(call) {
  return { id: call.id, type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments) } };
}
const finishReason = message => message.stopReason === 'toolUse' ? 'tool_calls' : message.stopReason === 'length' ? 'length' : 'stop';

export function completion(message, model, id = `chatcmpl-${randomUUID()}`) {
  const calls = message.content.filter(c => c.type === 'toolCall').map(toolCall);
  const text = message.content.filter(c => c.type === 'text').map(c => c.text).join('');
  const thinking = message.content.filter(c => c.type === 'thinking').map(c => c.thinking).join('');
  return { id, object: 'chat.completion', created: Math.floor(Date.now() / 1000), model,
    choices: [{ index: 0, message: { role: 'assistant', content: text || (calls.length ? null : ''),
      ...(calls.length ? { tool_calls: calls } : {}), ...(thinking ? { reasoning_content: thinking } : {}) },
    finish_reason: finishReason(message) }], usage: usageFor(message) };
}

export async function readCodexToken(path) {
  let auth;
  try { auth = JSON.parse(await readFile(path, 'utf8')); } catch { throw invalid('Cannot read Codex login; run codex login', 503); }
  if (auth.auth_mode !== 'chatgpt' || !auth.tokens?.access_token) {
    throw invalid('A ChatGPT subscription login is required; run codex login', 503);
  }
  const token = auth.tokens.access_token;
  let claims;
  try { claims = JSON.parse(Buffer.from(token.split('.')[1], 'base64url')); }
  catch { throw invalid('Invalid Codex login; run codex login', 503); }
  if (!Number.isFinite(claims.exp) || claims.exp * 1000 <= Date.now()) {
    throw invalid('Codex login expired; refresh it in Codex or run codex login', 503);
  }
  return token;
}

// Translate Codex's subscription windows to the header names arouter harvests.
export function quotaHeaders(headers, now = Math.floor(Date.now() / 1000)) {
  const h = new Headers(headers);
  const out = {};
  for (const [key, value] of h) {
    if (key.startsWith('x-ratelimit-') || key === 'retry-after') out[key] = value;
  }
  for (const [window, kind] of [['primary', 'requests'], ['secondary', 'tokens']]) {
    const raw = h.get(`x-codex-${window}-used-percent`);
    if (raw == null || !Number.isFinite(Number(raw))) continue;
    out[`x-ratelimit-${kind}-limit`] = '100';
    out[`x-ratelimit-${kind}-remaining`] = String(Math.max(0, 100 - Number(raw)));
    const reset = h.get(`x-codex-${window}-reset-after-seconds`);
    if (reset !== null && Number.isFinite(Number(reset))) {
      out[`x-ratelimit-reset-${kind}`] = String(now + Math.max(0, Math.ceil(Number(reset))));
    }
  }
  return out;
}

function json(res, status, body, headers = {}) {
  res.writeHead(status, { 'content-type': 'application/json', ...headers });
  res.end(JSON.stringify(body));
}

async function readBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > 2 * 1024 * 1024) throw invalid('Request exceeds 2 MiB', 413);
    chunks.push(chunk);
  }
  try { return JSON.parse(Buffer.concat(chunks)); } catch { throw invalid('Invalid JSON'); }
}

function authorized(header, key) {
  const actual = Buffer.from(header ?? '');
  const expected = Buffer.from(`Bearer ${key}`);
  return actual.length === expected.length && timingSafeEqual(actual, expected);
}

export function createBridge({ models, key, getToken, stream, normalizeContext, fetchImpl = globalThis.fetch, timeoutMs = 180_000 }) {
  if (!key || key.length < 32) throw new Error('Bridge key must have at least 32 characters');
  return createServer(async (req, res) => {
    const abort = new AbortController();
    const onClose = () => abort.abort();
    res.on('close', onClose);
    const timer = setTimeout(() => abort.abort(), timeoutMs);
    timer.unref();
    let upstreamStatus = 502;
    let responseHeaders = {};
    try {
      if (req.url === '/healthz' && req.method === 'GET') {
        await getToken();
        return json(res, 200, { status: 'ok', provider: 'codex-subscription' });
      }
      if (!authorized(req.headers.authorization, key)) throw invalid('Unauthorized', 401);
      if (req.url === '/v1/models' && req.method === 'GET') {
        return json(res, 200, { object: 'list', data: models.map(m => ({ id: m.id, object: 'model',
          owned_by: 'codex-subscription', context_length: m.contextWindow })) });
      }
      if (req.url !== '/v1/chat/completions' || req.method !== 'POST') throw invalid('Not found', 404);
      const body = await readBody(req);
      if (!body || typeof body !== 'object' || Array.isArray(body)) throw invalid('Expected a JSON object');
      const model = models.find(m => m.id === body.model);
      if (!model) throw invalid('Model is not enabled on this bridge');
      if (body.n !== undefined && body.n !== 1) throw invalid('Only n=1 is supported');
      if (body.response_format && body.response_format.type !== 'text') throw invalid('Structured response_format is not supported');
      for (const name of ['stop', 'top_p', 'logprobs', 'logit_bias', 'frequency_penalty', 'presence_penalty']) {
        if (body[name] != null) throw invalid(`${name} is not supported by this subscription bridge`);
      }
      if (body.stream !== undefined && typeof body.stream !== 'boolean') throw invalid('stream must be a boolean');
      const choice = body.tool_choice ?? 'auto';
      if (!['auto', 'none', 'required'].includes(choice) && !(choice?.type === 'function' && choice.function?.name)) {
        throw invalid('Invalid tool_choice');
      }
      const context = normalizeContext(toContext(body, model));
      const token = await getToken();
      const events = stream(model, context, { apiKey: token, transport: 'sse', maxRetries: 0,
        signal: abort.signal, timeoutMs, reasoningEffort: body.reasoning_effort ?? 'low',
        toolChoice: typeof choice === 'string' ? choice : 'auto',
        onPayload: payload => {
          if (typeof choice !== 'string') payload.tool_choice = { type: 'function', name: choice.function.name };
          if (body.parallel_tool_calls !== undefined) payload.parallel_tool_calls = Boolean(body.parallel_tool_calls);
          return payload;
        },
        fetch: async (url, options) => {
          const upstream = await fetchImpl(url, options);
          upstreamStatus = upstream.status;
          responseHeaders = quotaHeaders(upstream.headers);
          return upstream;
        },
      });
      const id = `chatcmpl-${randomUUID()}`;
      const created = Math.floor(Date.now() / 1000);
      let toolIndex = 0;
      let finished = false;
      const send = async (delta, reason = null, usage) => {
        if (res.destroyed) throw invalid('Client disconnected', 499);
        if (!res.headersSent) res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', ...responseHeaders });
        const chunk = { id, object: 'chat.completion.chunk', created, model: model.id,
          choices: usage ? [] : [{ index: 0, delta, finish_reason: reason }], ...(usage ? { usage } : {}) };
        if (!res.write(`data: ${JSON.stringify(chunk)}\n\n`)) await once(res, 'drain', { signal: abort.signal });
      };
      for await (const event of events) {
        if (event.type === 'error') {
          throw invalid('Codex subscription request failed; check Codex login and quota', upstreamStatus >= 400 ? upstreamStatus : 502);
        }
        if (event.type === 'done') {
          if (body.stream) {
            await send({}, finishReason(event.message));
            await send({}, null, usageFor(event.message));
            res.end('data: [DONE]\n\n');
          } else json(res, 200, completion(event.message, model.id, id), responseHeaders);
          finished = true;
          break;
        }
        if (!body.stream) continue;
        if (event.type === 'start') await send({ role: 'assistant', content: '' });
        if (event.type === 'text_delta') await send({ content: event.delta });
        if (event.type === 'thinking_delta') await send({ reasoning_content: event.delta });
        // Buffer only tool arguments until Pi has validated their final JSON.
        if (event.type === 'toolcall_end') await send({ tool_calls: [{ index: toolIndex++, ...toolCall(event.toolCall) }] });
      }
      if (!finished) throw invalid('Codex stream ended without a completion', 502);
    } catch (error) {
      abort.abort();
      if (res.destroyed) return;
      if (res.headersSent) res.destroy();
      else json(res, error.status ?? 502, { error: { type: 'codex_bridge_error',
        message: error.status ? error.message : 'Codex bridge request failed' } }, responseHeaders);
    } finally {
      clearTimeout(timer);
      res.off('close', onClose);
    }
  });
}

async function main() {
  const agentDir = process.env.PI_AGENT_DIR ?? join(homedir(), '.pi', 'agent');
  const version = (await readFile(join(agentDir, 'install/current-version'), 'utf8')).trim();
  if (!/^[\w.+-]+$/.test(version) || version === '.' || version === '..') throw new Error('Invalid Pi installation version');
  const packageDir = process.env.PI_AI_DIR ?? join(agentDir, 'install/releases', version, 'node_modules/@earendil-works/pi-ai');
  const base = join(packageDir, 'dist');
  const { stream } = await import(pathToFileURL(join(base, 'api/openai-codex-responses.js')));
  const { normalizeContext } = await import(pathToFileURL(join(base, 'utils/transcript.js')));
  const data = JSON.parse(await readFile(join(base, 'providers/data/openai-codex.json'), 'utf8'));
  const requested = (process.env.AROUTER_CODEX_MODELS ?? 'gpt-6-astra').split(',').map(s => s.trim());
  const available = Object.values(data['openai-codex-responses']);
  const models = requested.map(id => {
    const model = available.find(m => m.id === id);
    if (!model) throw new Error(`Model ${id} is absent from the installed Pi catalog`);
    return { ...model, baseUrl: 'https://chatgpt.com/backend-api', cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 } };
  });
  const configDir = join(homedir(), '.config/arouter');
  const key = (await readFile(process.env.AROUTER_CODEX_KEY_FILE ?? join(configDir, 'codex-bridge.key'), 'utf8')).trim();
  const authPath = process.env.CODEX_AUTH_FILE ?? join(homedir(), '.codex/auth.json');
  const getToken = () => readCodexToken(authPath);
  await getToken();
  const server = createBridge({ models, key, getToken, stream, normalizeContext });
  const port = Number(process.env.AROUTER_CODEX_PORT ?? 3930);
  server.listen(port, '127.0.0.1', () => console.log(`Codex subscription bridge: 127.0.0.1:${port}; models=${requested.join(',')}`));
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => {
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 5000).unref();
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(() => { console.error('Codex bridge startup failed: check Pi installation, bridge key, and codex login'); process.exitCode = 1; });
}
