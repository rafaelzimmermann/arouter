#!/usr/bin/env node
// Google generateContent compatibility endpoint backed by the signed-in
// Antigravity CLI. Requests are one-shot, isolated CLI turns; this is a
// compatibility adapter, not a Google Generative Language API credential shim.
// Function calling is emulated: tool declarations are rendered into the
// prompt and the CLI's structured JSON output is translated back into Google
// functionCall parts. generationConfig knobs the CLI cannot honor are ignored
// (reasoning effort maps to --effort, a response schema to --json-schema).
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

// Gemini's generationConfig carries knobs the Antigravity CLI cannot honor.
// Rejecting the whole request made the bridge unusable — arouter's adapter
// always sends maxOutputTokens because clients send max_tokens — so accept the
// object: map what the CLI exposes (reasoning effort) and ignore the rest.
function generationOptions(input) {
  const cfg = input.generationConfig ?? {};
  if (typeof cfg !== 'object' || Array.isArray(cfg)) throw bad('generationConfig must be an object');
  let effort;
  const thinking = cfg.thinkingConfig;
  if (thinking && typeof thinking === 'object') {
    if (typeof thinking.thinkingLevel === 'string') effort = thinking.thinkingLevel;
    else if (thinking.thinkingBudget !== undefined) {
      const budget = Number(thinking.thinkingBudget);
      if (!Number.isFinite(budget) || budget < 0) effort = 'high'; // dynamic budget
      else if (budget === 0 || budget <= 2048) effort = 'low';
      else if (budget <= 8192) effort = 'medium';
      else effort = 'high';
    }
  }
  // `--json-schema` covers Gemini's structured-output request.
  const structured = cfg.responseMimeType === 'application/json' && cfg.responseSchema && typeof cfg.responseSchema === 'object'
    ? cfg.responseSchema : null;
  return { effort, structured };
}

// arouter replays prior tool calls/results as functionCall/functionResponse
// parts. The CLI has no such protocol, so they become transcript lines; the
// declarations drive the JSON contract the model answers with.
function toolDeclarations(input) {
  if (input.toolConfig?.functionCallingConfig?.mode === 'NONE') return [];
  const decls = [];
  for (const tool of input.tools ?? []) {
    for (const fn of tool.functionDeclarations ?? []) {
      if (typeof fn?.name !== 'string' || !fn.name) throw bad('Each function declaration needs a name');
      decls.push({ name: fn.name, description: fn.description ?? '', parameters: fn.parameters ?? { type: 'object' } });
    }
  }
  return decls;
}

function promptFromGoogle(input) {
  if (!Array.isArray(input.contents) || input.contents.length === 0) throw bad('contents must be a non-empty array');
  const blocks = [];
  if (input.systemInstruction) {
    const parts = input.systemInstruction.parts ?? [];
    const text = parts.map((p) => p.text).filter((s) => typeof s === 'string');
    if (text.length !== parts.length) throw bad('Only text system instructions are supported', 501);
    if (text.length) blocks.push(`System instructions:\n${text.join('\n')}`);
  }
  for (const turn of input.contents) {
    if (!['user', 'model'].includes(turn.role ?? 'user')) throw bad('Only user and model roles are supported');
    if (!Array.isArray(turn.parts) || !turn.parts.length) throw bad('Each content turn needs at least one part');
    const text = [];
    for (const part of turn.parts) {
      if (typeof part.text === 'string') {
        text.push(part.text);
      } else if (part.functionCall) {
        text.push(`Assistant called ${part.functionCall.name}(${JSON.stringify(part.functionCall.args ?? {})})`);
      } else if (part.functionResponse) {
        text.push(`Tool ${part.functionResponse.name} returned ${JSON.stringify(part.functionResponse.response ?? {})}`);
      } else {
        throw bad('Antigravity bridge supports text and tool parts only', 501);
      }
    }
    blocks.push(`${turn.role === 'model' ? 'Assistant' : 'User'}:\n${text.join('\n')}`);
  }
  // Keep this a single model answer. In particular, never pass client content
  // as an instruction to use Antigravity's filesystem, terminal, or MCP tools.
  blocks.push('Answer the conversation directly. Do not access files or perform actions yourself.');
  return blocks.join('\n\n');
}

// The JSON contract for emulated function calling: prose in `text`, calls in
// `tool_calls`. `--json-schema` makes the CLI return `structured_output`.
function toolInstruction(decls) {
  const lines = decls.map((d) => `- ${d.name}: ${d.description}\n  parameters: ${JSON.stringify(d.parameters)}`);
  return ['You may call the functions below, but you cannot execute them.',
    'Answer as JSON. Put prose in "text"; to call functions add "tool_calls"',
    '(an array of {"name", "arguments"}). Use only the listed names.',
    'Functions:', ...lines].join('\n');
}

function toolResponseSchema(decls) {
  return {
    type: 'object',
    properties: {
      text: { type: 'string' },
      tool_calls: {
        type: 'array',
        items: {
          type: 'object',
          properties: {
            name: { type: 'string', enum: decls.map((d) => d.name) },
            arguments: { type: 'object' },
          },
          required: ['name'],
        },
      },
    },
  };
}

function runCli({ cli, model, prompt, cwd, timeoutMs, signal, effort, jsonSchema }) {
  return new Promise((resolve, reject) => {
    const args = ['--print', prompt, '--model', model, '--output-format', 'json',
      '--print-timeout', `${Math.ceil(timeoutMs / 1000)}s`, '--sandbox', '--mode', 'plan', '--disable-slash-commands'];
    if (effort) args.push('--effort', effort);
    if (jsonSchema) args.push('--json-schema', JSON.stringify(jsonSchema));
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

function googleResponse(result, model, { toolMode = false, structured = null } = {}) {
  const parts = [];
  if (toolMode) {
    const out = result.structured_output ?? {};
    if (typeof out.text === 'string' && out.text.trim()) parts.push({ text: out.text });
    for (const call of Array.isArray(out.tool_calls) ? out.tool_calls : []) {
      if (typeof call?.name !== 'string' || !call.name) continue;
      const args = call.arguments && typeof call.arguments === 'object' ? call.arguments : {};
      parts.push({ functionCall: { name: call.name, args } });
    }
    // Never report an empty turn as success: fall back to the raw response.
    if (!parts.length) parts.push({ text: typeof result.response === 'string' ? result.response : '' });
  } else if (structured && result.structured_output !== undefined) {
    parts.push({ text: JSON.stringify(result.structured_output) });
  } else {
    parts.push({ text: String(result.response ?? '') });
  }
  const promptTokens = Number(result.usage?.input_tokens ?? 0);
  const outputTokens = Number(result.usage?.output_tokens ?? 0);
  return {
    candidates: [{ content: { role: 'model', parts }, finishReason: 'STOP', index: 0 }],
    usageMetadata: { promptTokenCount: promptTokens, candidatesTokenCount: outputTokens,
      totalTokenCount: Number(result.usage?.total_tokens ?? promptTokens + outputTokens) },
    modelVersion: model,
  };
}

export function createBridge({ key, models, cli = '/home/spike/.local/bin/agy', timeoutMs = 180_000, concurrency = 2, run = runCli }) {
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
      const options = generationOptions(input);
      const decls = toolDeclarations(input);
      const toolMode = decls.length > 0;
      const schema = toolMode ? toolResponseSchema(decls) : options.structured;
      const prompt = toolMode
        ? `${promptFromGoogle(input)}\n\n${toolInstruction(decls)}`
        : promptFromGoogle(input);
      if (active >= concurrency) throw bad('Antigravity bridge is at capacity', 429);
      active++;
      let result;
      let workdir;
      try {
        workdir = await mkdtemp(join(tmpdir(), 'arouter-agy-'));
        result = await run({ cli, model, prompt, cwd: workdir, timeoutMs, signal: abort.signal,
          effort: options.effort, jsonSchema: schema });
      } finally {
        active--;
        if (workdir) await rm(workdir, { recursive: true, force: true });
      }
      const response = googleResponse(result, model, { toolMode, structured: options.structured });
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
