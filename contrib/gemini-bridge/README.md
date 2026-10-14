# Gemini CLI subscription bridge

This bridge forwards Google's `generateContent` and streaming APIs using the
OAuth login created by Gemini CLI. arouter translates OpenAI chat requests to
Google's JSON shape and sends the bridge key in `x-goog-api-key`; Google access
tokens are read from `~/.gemini/oauth_creds.json` and refreshed by the bridge.
No Google API key is used or stored.

## Prerequisites

- Node.js 20 or newer.
- Gemini CLI installed and signed in as the account whose subscription should
  serve requests. Its OAuth file must be `~/.gemini/oauth_creds.json` or be
  selected with `GEMINI_AUTH_FILE`.
- arouter built with `wire = "gemini"` support.

The bridge refuses to start if the OAuth file has no refresh token. It does not
create or copy credentials. Google refresh token rotation is written back to
that OAuth file with mode `0600`.

## Run as a user service

Install the bridge in `~/.local/share/arouter/gemini-bridge/bridge.mjs` and
create a random key in `~/.config/arouter/gemini-bridge.key` with mode `0600`.
Save this unit as `~/.config/systemd/user/arouter-gemini-bridge.service`,
adjusting the bind address for the Docker host gateway:

```ini
[Unit]
Description=arouter Gemini subscription bridge
After=network-online.target

[Service]
ExecStart=/usr/bin/node %h/.local/share/arouter/gemini-bridge/bridge.mjs
Environment=AROUTER_GEMINI_MODELS=gemini-2.5-pro,gemini-2.5-flash
Environment=AROUTER_GEMINI_HOST=172.18.0.1
Restart=on-failure
RestartSec=3
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
```

Run `systemctl --user daemon-reload` and
`systemctl --user enable --now arouter-gemini-bridge`.

## arouter models

For each model, use the same bridge key in arouter's environment and set the
base URL to the bridge. The model ids below are examples; enable only models
present in `AROUTER_GEMINI_MODELS`.

```toml
[[models]]
id = "google/gemini-2.5-pro"
grade = "A+"
pool = ["quota"]
wire = "gemini"
base_url = "http://172.18.0.1:3940/v1beta"
auth = "env:AROUTER_GEMINI_BRIDGE_KEY"
context = 1048576
```

Check `http://172.18.0.1:3940/healthz` from the arouter container and
`systemctl --user status arouter-gemini-bridge` on the host.

## Settings

| Variable | Default |
| --- | --- |
| `AROUTER_GEMINI_MODELS` | `gemini-2.5-pro,gemini-2.5-flash` |
| `AROUTER_GEMINI_HOST` | `127.0.0.1` |
| `AROUTER_GEMINI_PORT` | `3940` |
| `AROUTER_GEMINI_KEY_FILE` | `~/.config/arouter/gemini-bridge.key` |
| `GEMINI_AUTH_FILE` | `~/.gemini/oauth_creds.json` |
| `GEMINI_CLI_API_KEY` | unset; optional public CLI API key |
| `GEMINI_CLI_USER_AGENT` | `gemini-cli/unknown` |

Run the bridge tests with `node --experimental-test-isolation=none --test
contrib/gemini-bridge/bridge.test.mjs`.
