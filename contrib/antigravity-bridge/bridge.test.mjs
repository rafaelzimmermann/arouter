// node --experimental-test-isolation=none --test contrib/antigravity-bridge/bridge.test.mjs
// The CLI is injected, so these tests never spawn `agy`.
import { test } from 'node:test';
import assert from 'node:assert';
import { createBridge } from './bridge.mjs';

const KEY = 'a'.repeat(40);
const MODELS = ['gemini-3.8-flash-low'];

// Capture the argv-ish options the bridge hands to the CLI runner and answer
// with a canned CLI result.
function fakeCli(result) {
  const calls = [];
  return { calls, run: async (options) => { calls.push(options); return result; } };
}

async function serve(run) {
  const server = createBridge({ key: KEY, models: MODELS, run });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  return { origin: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

async function generate(origin, body) {
  const response = await fetch(`${origin}/v1beta/models/${MODELS[0]}:generateContent`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-goog-api-key': KEY },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}

const plainResult = {
  status: 'SUCCESS', response: 'hi', usage: { input_tokens: 3, output_tokens: 1, total_tokens: 4 },
};

test('plain text chat still works', async () => {
  const cli = fakeCli(plainResult);
  const { origin, close } = await serve(cli.run);
  try {
    const { status, body } = await generate(origin, { contents: [{ role: 'user', parts: [{ text: 'hi' }] }] });
    assert.equal(status, 200);
    assert.deepEqual(body.candidates[0].content.parts, [{ text: 'hi' }]);
  } finally {
    close();
  }
});

test('generationConfig is accepted; effort maps to --effort', async () => {
  const cli = fakeCli(plainResult);
  const { origin, close } = await serve(cli.run);
  try {
    const { status } = await generate(origin, {
      contents: [{ role: 'user', parts: [{ text: 'hi' }] }],
      generationConfig: { maxOutputTokens: 64, temperature: 0.2, thinkingConfig: { thinkingLevel: 'high' } },
    });
    assert.equal(status, 200, 'maxOutputTokens must not 501 the request');
    assert.equal(cli.calls[0].effort, 'high');
    assert.equal(cli.calls[0].jsonSchema, null);
  } finally {
    close();
  }
});

test('tool declarations become a JSON schema and functionCall parts', async () => {
  const cli = fakeCli({
    status: 'SUCCESS', response: 'ok',
    structured_output: { text: 'let me check', tool_calls: [{ name: 'get_weather', arguments: { city: 'Paris' } }] },
    usage: { input_tokens: 10, output_tokens: 4, total_tokens: 14 },
  });
  const { origin, close } = await serve(cli.run);
  try {
    const { status, body } = await generate(origin, {
      contents: [{ role: 'user', parts: [{ text: 'weather in Paris?' }] }],
      tools: [{ functionDeclarations: [{ name: 'get_weather', description: 'Look up weather',
        parameters: { type: 'object', properties: { city: { type: 'string' } } } }] }],
    });
    assert.equal(status, 200);
    const schema = cli.calls[0].jsonSchema;
    assert.equal(schema.properties.tool_calls.items.properties.name.enum[0], 'get_weather');
    assert.match(cli.calls[0].prompt, /get_weather/);
    assert.deepEqual(body.candidates[0].content.parts, [
      { text: 'let me check' },
      { functionCall: { name: 'get_weather', args: { city: 'Paris' } } },
    ]);
  } finally {
    close();
  }
});

test('functionCallingConfig NONE drops the tools', async () => {
  const cli = fakeCli(plainResult);
  const { origin, close } = await serve(cli.run);
  try {
    const { status } = await generate(origin, {
      contents: [{ role: 'user', parts: [{ text: 'hi' }] }],
      tools: [{ functionDeclarations: [{ name: 'get_weather' }] }],
      toolConfig: { functionCallingConfig: { mode: 'NONE' } },
    });
    assert.equal(status, 200);
    assert.equal(cli.calls[0].jsonSchema, null);
  } finally {
    close();
  }
});

test('replayed tool calls and results are transcript lines, not 501', async () => {
  const cli = fakeCli(plainResult);
  const { origin, close } = await serve(cli.run);
  try {
    const { status } = await generate(origin, {
      contents: [
        { role: 'user', parts: [{ text: 'weather?' }] },
        { role: 'model', parts: [{ functionCall: { name: 'get_weather', args: { city: 'Paris' } } }] },
        { role: 'user', parts: [{ functionResponse: { name: 'get_weather', response: { temp: 21 } } }] },
      ],
    });
    assert.equal(status, 200);
    assert.match(cli.calls[0].prompt, /get_weather/);
    assert.match(cli.calls[0].prompt, /temp/);
  } finally {
    close();
  }
});

test('a client response schema is forwarded to --json-schema', async () => {
  const cli = fakeCli({
    status: 'SUCCESS', response: '{"ok":true}', structured_output: { ok: true },
    usage: { input_tokens: 2, output_tokens: 1, total_tokens: 3 },
  });
  const { origin, close } = await serve(cli.run);
  try {
    const { status, body } = await generate(origin, {
      contents: [{ role: 'user', parts: [{ text: 'json please' }] }],
      generationConfig: { responseMimeType: 'application/json', responseSchema: { type: 'object' } },
    });
    assert.equal(status, 200);
    assert.deepEqual(cli.calls[0].jsonSchema, { type: 'object' });
    assert.deepEqual(body.candidates[0].content.parts, [{ text: '{"ok":true}' }]);
  } finally {
    close();
  }
});

test('image parts stay unsupported', async () => {
  const cli = fakeCli(plainResult);
  const { origin, close } = await serve(cli.run);
  try {
    const { status } = await generate(origin, {
      contents: [{ role: 'user', parts: [{ inlineData: { mimeType: 'image/png', data: 'AAAA' } }] }],
    });
    assert.equal(status, 501);
    assert.equal(cli.calls.length, 0, 'an unsupported request never reaches the CLI');
  } finally {
    close();
  }
});
