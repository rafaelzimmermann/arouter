#!/usr/bin/env node
// Google generateContent compatibility endpoint backed by the signed-in
// Antigravity CLI. Requests are one-shot, isolated CLI turns; this is a
// text-only adapter, not a Google Generative Language API credential shim.
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { timingSafeEqual } from 'node:crypto';

const bad = (message, status = 400) => Object.assign(new Error(message), { status });

function json(res, status, value) {
  res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
  res.end(`${JSON.stringify(value)}\n`);
}

function authorized(headers, key) {
  const supplied = headers['x-goog-api-key'] ?? /^Bearer (.+)$/.exec(headers.authorization ?? '')?.[1] ?? '';
  const a = Buffer.from(String(supplied));
  const b = Buffer.from(key);
  return b.length >= 32 && a.length === b.length && timingSafeEqual(a, b);
}

async function body(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > 8 * 1024 * 1024) throw bad('Request body too large', 413);
    chunks.push(chunk);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); }
  catch { throw bad('Expected a JSON body'); }
}

function promptFromGoogle(input) {
  if (!Array.isArray(input.contents) || input.contents.length === 0) throw bad('contents must be a non-empty array');
  if (input.tools || input.toolConfig) throw bad('Antigravity bridge does not support function calling', 501);
  if (input.generationConfig && Object.keys(input.generationConfig).length) {
    throw bad('Antigravity CLI does not expose Gemini generationConfig controls', 501);
  }
  if (input.safetySettings) throw bad('Antigravity bridge does not expose Gemini safety settings', 501);
  const blocks = [];
  if (input.systemInstruction) {
    const text = (input.systemInstruction.parts ?? []).map((p) => p.text).filter((s) => typeof s === 'string');
    if (text.length !== (input.systemInstruction.parts ?? []).length) throw bad('Only text system instructions are supported', 501);
    if (text.length) blocks.push(`System instructions:\n${text.join('\n')}`);
  }
  for (const turn of input.contents) {
    if (!['user', 'model'].includes(turn.role ?? 'user')) throw bad('Only user and model roles are supported');
    if (!Array.isArray(turn.parts) || !turn.parts.length) throw bad('Each content turn needs at least one part');
    const text = [];
    for (const part of turn.parts) {
      if (typeof part.text !== 'string') throw bad('Antigravity bridge supports text parts only', 501);
      text.push(part.text);
    }
    blocks.push(`${turn.role === 'model' ? 'Assistant' : 'User'}:\n${text.join('\n')}`);
  }
  // Keep this a single model answer. In particular, never pass client content
  // as an instruction to use Antigravity's filesystem, terminal, or MCP tools.
  blocks.push('Answer the conversation directly. Do not use tools, access files, or perform actions.');
  return blocks.join('\n\n');
}

function runCli({ cli, model, prompt, cwd, timeoutMs, signal }) {
  return new Promise((resolve, reject) => {
    const args = ['--print', prompt, '--model', model, '--output-format', 'json',
      '--print-timeout', `${Math.ceil(timeoutMs / 1000)}s`, '--sandbox', '--mode', 'plan', '--disable-slash-commands'];
    const child = spawn(cli, args, { cwd, stdio: ['ignore', 'pipe', 'pipe'], env: process.env });
    let stdout = '';
    let stderr = '';
    let settled = false;
    const finish = (fn, value) => { if (settled) return; settled = true; clearTimeout(timer); signal?.removeEventListener('abort', abort); fn(value); };
    const abort = () => child.kill('SIGTERM');
    const timer = setTimeout(() => { child.kill('SIGKILL'); finish(reject, bad('Antigravity request timed out', 504)); }, timeoutMs + 3000);
    signal?.addEventListener('abort', abort, { once: true });
    child.stdout.on('data', (chunk) => { stdout += chunk; if (stdout.length > 16 * 1024 * 1024) child.kill('SIGKILL'); });
    child.stderr.on('data', (chunk) => { stderr += chunk; if (stderr.length > 64 * 1024) stderr = stderr.slice(-64 * 1024); });
    child.on('error', () => finish(reject, bad('Antigravity CLI could not be started', 503)));
    child.on('close', (code) => {
      if (settled) return;
      if (signal?.aborted) return finish(reject, bad('Client disconnected', 499));
      if (code !== 0) return finish(reject, bad('Antigravity CLI request failed', 502));
      try {
        const output = JSON.parse(stdout);
        if (output.status !== 'SUCCESS' || typeof output.response !== 'string') throw new Error('invalid result');
        finish(resolve, output);
      } catch {
        finish(reject, bad('Antigravity CLI returned an invalid response', 502));
      }
    });
  });
}

function googleResponse(result, model) {
  const text = result.response;
  const promptTokens = Number(result.usage?.input_tokens ?? 0);
  const outputTokens = Number(result.usage?.output_tokens ?? 0);
  return {
    candidates: [{ content: { role: 'model', parts: [{ text }] }, finishReason: 'STOP', index: 0 }],
    usageMetadata: { promptTokenCount: promptTokens, candidatesTokenCount: outputTokens,
      totalTokenCount: Number(result.usage?.total_tokens ?? promptTokens + outputTokens) },
    modelVersion: model,
  };
}

export function createBridge({ key, models, cli = '/home/spike/.local/bin/agy', timeoutMs = 180_000, concurrency = 2 }) {
  let active = 0;
  return createServer(async (req, res) => {
    const abort = new AbortController();
    res.on('close', () => { if (!res.writableEnded) abort.abort(); });
    try {
      if (req.method === 'GET' && req.url === '/healthz') return json(res, 200, { status: 'ok', provider: 'antigravity-cli' });
      if (!authorized(req.headers, key)) throw bad('Unauthorized', 401);
      if (req.method === 'GET' && req.url === '/v1beta/models') {
        return json(res, 200, { models: models.map((id) => ({ name: `models/${id}`, displayName: id,
          supportedGenerationMethods: ['generateContent', 'streamGenerateContent'], inputTokenLimit: 200_000,
          outputTokenLimit: 32_000 })) });
      }
      const url = new URL(req.url, 'http://bridge');
      const match = /^\/v1beta\/models\/([^/:]+):(generateContent|streamGenerateContent)$/.exec(url.pathname);
      if (!match || req.method !== 'POST') throw bad('Not found', 404);
      const [, model, method] = match;
      if (!models.includes(model)) throw bad('Model is not enabled on this bridge', 404);
      if (url.searchParams.has('alt') && url.searchParams.get('alt') !== 'sse') throw bad('Only alt=sse is supported');
      const input = await body(req);
      const prompt = promptFromGoogle(input);
      if (active >= concurrency) throw bad('Antigravity bridge is at capacity', 429);
      active++;
      let result;
      let workdir;
      try {
        workdir = await mkdtemp(join(tmpdir(), 'arouter-agy-'));
        result = await runCli({ cli, model, prompt, cwd: workdir, timeoutMs, signal: abort.signal });
      } finally {
        active--;
        if (workdir) await rm(workdir, { recursive: true, force: true });
      }
      const response = googleResponse(result, model);
      if (method === 'generateContent') return json(res, 200, response);
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
      res.end(`data: ${JSON.stringify(response)}\n\n`);
    } catch (error) {
      if (res.destroyed) return;
      const status = error.status ?? 502;
      json(res, status, { error: { code: status, status, message: error.status ? error.message : 'Antigravity bridge failed' } });
    }
  });
}

async function main() {
  const keyPath = process.env.AROUTER_GEMINI_KEY_FILE ?? join(process.env.HOME ?? '/home/spike', '.config/arouter/gemini-bridge.key');
  const { readFile } = await import('node:fs/promises');
  const key = (await readFile(keyPath, 'utf8')).trim();
  const models = (process.env.AROUTER_GEMINI_MODELS ?? 'gemini-3.8-flash-low').split(',').map((s) => s.trim()).filter(Boolean);
  const server = createBridge({ key, models, cli: process.env.ANTIGRAVITY_CLI ?? '/home/spike/.local/bin/agy',
    timeoutMs: Number(process.env.AROUTER_GEMINI_TIMEOUT_MS ?? 180_000), concurrency: Number(process.env.AROUTER_GEMINI_CONCURRENCY ?? 2) });
  const host = process.env.AROUTER_GEMINI_HOST ?? '127.0.0.1';
  const port = Number(process.env.AROUTER_GEMINI_PORT ?? 3940);
  server.listen(port, host, () => console.log(`Antigravity Gemini bridge: ${host}:${port}; models=${models.join(',')}`));
  for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => server.close(() => process.exit(0)));
}

if (process.argv[1] && new URL(import.meta.url).pathname === process.argv[1]) {
  main().catch(() => { console.error('Antigravity bridge startup failed'); process.exitCode = 1; });
}
