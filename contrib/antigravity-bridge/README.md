# Antigravity local Gemini bridge

This host service exposes a Google `generateContent` shaped endpoint for
arouter, then runs each request through the signed-in Antigravity CLI. It
does not read or copy Antigravity credentials. Requests run in a fresh
temporary working directory with CLI sandboxing and plan mode enabled.
Streaming requests return one SSE event after the answer is complete.

Function calling is emulated: tool declarations are rendered into the prompt
and the CLI's `--json-schema` structured output is translated back into Google
`functionCall` parts. Reasoning effort (`thinkingConfig`) maps to `--effort`;
a JSON response schema maps to `--json-schema`; other `generationConfig` knobs
(`temperature`, `maxOutputTokens`, …) and `safetySettings` are accepted and
ignored because the CLI does not expose them. Image and file parts still
receive an explicit `501`.

## Install

Install the official Antigravity CLI for the same user that owns the signed-in
Antigravity session. Verify `agy models` and a harmless `agy --print ...`
request as that user first. The service expects the CLI at
`~/.local/bin/agy`; set `ANTIGRAVITY_CLI` to override it.

Copy `bridge.mjs` to `~/.local/share/arouter/antigravity-bridge/bridge.mjs`.
Create a key and protect it:

```sh
install -d -m 700 ~/.config/arouter ~/.config/systemd/user
umask 077
openssl rand -hex 32 > ~/.config/arouter/gemini-bridge.key
```

Save this unit as `~/.config/systemd/user/arouter-gemini-bridge.service`,
setting `AROUTER_GEMINI_HOST` to the Docker host gateway used by arouter:

```ini
[Unit]
Description=arouter Antigravity Gemini bridge
After=network-online.target

[Service]
ExecStart=/usr/bin/node %h/.local/share/arouter/antigravity-bridge/bridge.mjs
Environment=AROUTER_GEMINI_MODELS=gemini-3.8-flash-low
Environment=AROUTER_GEMINI_HOST=172.18.0.1
Environment=AROUTER_GEMINI_PORT=3940
Environment=ANTIGRAVITY_CLI=%h/.local/bin/agy
Restart=on-failure
RestartSec=3
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
```

Then run `systemctl --user daemon-reload` and
`systemctl --user enable --now arouter-gemini-bridge`.

## arouter model

Set the same key in arouter's environment as `AROUTER_GEMINI_BRIDGE_KEY` and
configure the matching model:

```toml
[[models]]
id = "google/gemini-3.8-flash-low"
grade = "A"
pool = ["quota"]
wire = "gemini"
base_url = "http://172.18.0.1:3940/v1beta"
auth = "env:AROUTER_GEMINI_BRIDGE_KEY"
context = 200000
```

The bridge process must run on the same host and as the same user as the
Antigravity login. Keep it off public interfaces; the random bridge key is
required for every model request. The CLI controls upstream model access and
usage. Text chat and emulated function calling work; images and files are
intentionally unsupported by this CLI adapter.

Run the bridge tests with `node --experimental-test-isolation=none --test
contrib/antigravity-bridge/bridge.test.mjs`.
