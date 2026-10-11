# Codex subscription bridge

Optional loopback upstream for arouter when using an existing Codex ChatGPT
subscription login. The Rust router currently sends Chat Completions requests;
this helper translates them through the Codex Responses adapter in the installed
Pi package. It supports buffered and streaming responses, function tools, base64
images, token usage, and subscription quota headers. It does not execute tools.

The bridge binds **127.0.0.1:3930**, requires its own bearer key, and advertises
only explicitly enabled models. arouter owns grade/pool selection and failover.
Subscription models belong in `pool = ["quota"]`, with zero per-token cost in
arouter's accounting. Separately billed OpenAI API credentials are rejected.

## Requirements

- Node.js supporting ES modules and built-in `fetch` (tested with Node 26).
- A managed Pi installation (tested with `@earendil-works/pi-ai` 1.1.0).
- An existing `codex login` using ChatGPT, stored in `~/.codex/auth.json`.
- A random local bridge key of at least 32 characters, stored with mode 0600
  at `~/.config/arouter/codex-bridge.key`.

The bridge reads the Codex auth file on every request. **Codex owns login and
token renewal**: this helper does not copy or rotate subscription credentials.
If the login expires without Codex renewing it, requests return 503 until the
login is refreshed. The bridge key is a separate local access token.

## Run

```sh
node contrib/codex-bridge/bridge.mjs
```

For a durable installation, copy `bridge.mjs` to
`~/.local/share/arouter/codex-bridge/` and use a systemd user service:

```ini
[Unit]
Description=arouter Codex subscription bridge
After=network-online.target

[Service]
ExecStart=/usr/bin/node %h/.local/share/arouter/codex-bridge/bridge.mjs
Environment=AROUTER_CODEX_MODELS=gpt-6-astra
Restart=on-failure
RestartSec=3
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
```

Save as `~/.config/systemd/user/arouter-codex-bridge.service`, then run
`systemctl --user daemon-reload` and
`systemctl --user enable --now arouter-codex-bridge`.

Use the same local bridge key in arouter, preferably through its env keystore:

```toml
[profiles.codex]
description = "Codex subscription only"
models = { include = ["codex/*"] }

[[models]]
id = "codex/gpt-6-astra"
grade = "A+"
pool = ["quota"]
context = 272000
base_url = "http://127.0.0.1:3930/v1"
auth = "env:AROUTER_CODEX_BRIDGE_KEY"
weight = 1.0
cost_in = 0.0
cost_out = 0.0
```

Set `AROUTER_CODEX_BRIDGE_KEY` in arouter's `.env` to the contents of the bridge
key file. A protected config can alternatively use `auth = "key:..."`.
Reload the router after changing config. Requests for `A+` can use Codex through
the default quota pool; `/codex/v1/chat/completions` selects only this provider.
The profile is not a separate authorization boundary.

The current native quota implementation defaults unknown providers to header
harvesting. The bridge maps Codex's primary and secondary subscription windows
into the request/token rate-limit headers that arouter understands. It only
learns quota from actual inference responses; it does not poll quota endpoints.

## Settings

| Environment variable | Default |
| --- | --- |
| `AROUTER_CODEX_MODELS` | `gpt-6-astra` (comma-separated allowlist) |
| `AROUTER_CODEX_PORT` | `3930`; host remains loopback |
| `AROUTER_CODEX_KEY_FILE` | `~/.config/arouter/codex-bridge.key` |
| `CODEX_AUTH_FILE` | `~/.codex/auth.json` |
| `PI_AGENT_DIR` | `~/.pi/agent` |
| `PI_AI_DIR` | Package inside the managed Pi current release |

Model metadata comes from the installed Pi catalog. The bridge does not imply
that every catalog model is available to every subscription. It forwards to the
fixed ChatGPT Codex backend, never to the separately billed OpenAI API.

Structured response formats, remote image URLs, multiple completions, stop
sequences, logprobs, and sampling penalties are not supported. Output token caps
(`max_tokens`/`max_completion_tokens`) are not enforced by this adapter; the
installed Pi Codex transport does not send such a cap to the subscription
backend. Requests have a three-minute deadline and default to low reasoning.
Tool arguments are buffered until complete; text and reasoning deltas stream
immediately. Errors after streaming starts terminate the connection instead of
pretending that an incomplete response finished successfully.

## Checks

```sh
node --test --test-isolation=none contrib/codex-bridge/bridge.test.mjs
systemctl --user status arouter-codex-bridge
curl --fail http://127.0.0.1:3930/healthz
```

Tests use fake credentials and local mock streams. They cover tool history,
cached usage, quota normalization, auth/expiry, allowlisted discovery, JSON/SSE
responses, HTTP 429 propagation, client cancellation, and timeout cancellation.
`/healthz` verifies local login freshness, not upstream network availability or
remaining subscription quota. The helper depends on Pi's internal adapter API;
rerun its tests and a small live request after upgrading Pi.
