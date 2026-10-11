# arouter

**Pick a grade, not a provider.** A quota-aware LLM agent router (Rust): expose `arouter:A+ / A / B` (plus `-local` and `-ondemand` pools) to pi and any OpenAI-compatible client, and let the daemon route each request to the best available upstream — subscriptions, free-tier quotas, local engines, or budget-guarded pay-per-token.

- **Grade-based UX** — `/model` in pi shows school grades; provider names never leak into the picker.
- **Quota-aware** — ingests `aitop --json`: rolling windows, reset times, pace; avoids exhausted/scarce keys, preserves near-exhausted quota, cools down after 429s.
- **Budget-guarded on-demand** — daily/monthly caps, hourly pace, per-request preflight estimates; `$ spent` visible everywhere.
- **Auto-graded catalog** — well-known models (GPT, Claude, GLM, Gemini, Qwen, Llama…) get school grades out of the box: curated embedded catalog + price/keyword heuristic for the long tail; manual overrides always win.
- **Profiles** — `arouter-night-ondemand:A+`: per-task overlays with their own fallback ladder, budgets and model visibility; models can be exclusive to a profile.
- **Multi-user** — `[[callers]]` keys attribute every request to a user/project: per-caller usage stats, budgets (E4), monitor page (E5).
- **Prometheus** — `/metrics` exports requests, tokens, $ spend, latency histograms and TTFB, all labeled by caller/project/pool/grade.
- **`arouter --monitor`** — htop-style TUI: network-switch LED panel per provider + charts per model.
- **Single binary, localhost daemon** — `arouter serve` via systemd user unit; `arouter init` wires config, pi `models.json`, and the unit automatically.

## Status

v0.1 — feature-complete and battle-tested on a daily-driver setup: grade routing with failover, quota-aware scoring (exhausted/scarce/reset-soon/pace/drain), budget-guarded on-demand, profiles, multi-user callers with stacked caps, Anthropic-wire support for Claude Code, local-engine awareness (warm-cache stickiness, residency, busy avoidance), an htop-style monitor TUI, `install.sh` + `arouter init` + `arouter doctor` for setup, and a 37-check Docker integration rig with a provisioned Prometheus+Grafana observability stack.

## Usage

```bash
cargo build --release
./target/release/arouter serve --config config.example.toml   # daemon on 127.0.0.1:3929
curl -s localhost:3929/v1/chat/completions -d '{"model":"A+","messages":[{"role":"user","content":"hi"}]}'
curl -s localhost:3929/metrics | grep arouter_requests_total     # Prometheus
curl -s localhost:3929/v1/models                                 # → A+, A, B
./target/release/arouter models --config config.example.toml    # grades + provenance of every model
./target/release/arouter models --write --config config.example.toml  # pin them to grades.auto.toml
kill -HUP $(pgrep -x arouter)                                    # hot-reload after edits
```

Observability: `/metrics` speaks Prometheus (per-pool/caller requests, tokens,
estimated spend, degraded serves, latency/TTFB histograms, quota windows,
drain countdown). Import [contrib/grafana-dashboard.json](contrib/grafana-dashboard.json)
and point it at a Prometheus scraping the daemon to get the matching dashboard.
Two ways to run it:

```sh
docker compose -p arouter-obs -f contrib/observability/docker-compose.yml up -d --wait
# persistent stack against the LIVE daemon (127.0.0.1:3929) — Prometheus
# host-networks so a loopback-bound daemon needs no changes:
#   Grafana http://localhost:3300  ·  Prometheus http://localhost:9090

KEEP=1 ./tests/integration/run.sh    # or: the E9 rig's sidecars (37-check
# matrix incl. observability smoke tests), same ports, ephemeral
```

Failing models cool down (30s→2m→8m→30m) and eventually trip a circuit breaker; `/status.json` shows a `health` section per model. Discovery polls upstreams (`/v1/models`, ollama `/api/tags`; OpenRouter pricing included) every `[routing] discovery_interval_secs` — new upstream models appear automatically, graded by catalog or heuristic, and never outrank manual entries.

**Local engine smarts (E8)**: `[local_engines]` polls ollama (`/api/ps`) and llama.cpp/strata (`/slots`) so requests prefer models that are already resident (no multi-second cold loads), skip engines mid-generation, and stick to the model that just served a caller (warm KV cache, `routing.sticky_local_secs`). All boosts stay grade-first and pool-bound; `/status.json` → `local_engines`.

**Quota awareness (E3)**: arouter execs `[quota_watcher] command` (default `aitop --json`, every 60s) and scores each provider: exhausted windows are skipped until their reset (arouter's own 429 then carries a reset-aware `Retry-After`), scarce windows (>80%) and "pace ahead" burn rates are deprioritized, equally-available providers spread by remaining headroom `(100−pct)×hours-to-reset`, and on-demand requests are served cheapest-first within the requested grade. Leftover quota on a window resetting within 4h is a wasting asset: the provider enters **drain mode** — penalties lift and it wins ties within its grade (soonest reset first) so the quota gets spent instead of wasted. Live numbers land in `/status.json` → `quota` and as `arouter_quota_window_pct` gauges.

**On-demand budgets (E4)**: chargeable attempts pass stacked spend guards before any request leaves arouter — `[pool.ondemand.budget]` (daily/monthly/rolling-hourly pace/per-request), per-profile budgets, and `[[callers]].budget_usd_day` personal caps, tightest wins. Ordering is cheapest-first within the requested grade, so guards are the backstop; refusals are 402 JSON naming the exact layer/window/spend, and responses carry `X-Arouter-Cost-Usd` + `X-Arouter-Budget-Remaining-Today`. Falling back from the quota pool into a paid one is explicit opt-in only (`cross_pool = ["ondemand"]`); such responses carry `X-Arouter-Cross-Pool: quota` so charges are always visible.

Grades accept ranges per request: `"model": "A+..A"`. Pools: `/v1` (quota, default), `/local/v1`, `/ondemand/v1`; profiles: `/{profile}/v1` (e.g. `vip/v1`). Every response tells you where it went: `X-Arouter-Upstream`, `X-Arouter-Grade`, `X-Arouter-Pool`, `X-Arouter-Degraded` (grade dropped).

### Multiple developers / agents

```toml
[auth]
require = false          # true once you bind beyond loopback

[[callers]]
name    = "alice"
project = "arouter"
key     = "env:AROUTER_KEY_ALICE"   # client sends: Authorization: Bearer <key>
```

Then `sum by (caller) (increase(arouter_cost_usd_total[1d]))` in Grafana = spend per person; `sum by (project, model) (...)` = per-project model mix. `arouter --monitor` gets a per-caller page in E5. arouter is plain HTTP — expose it only on trusted networks (LAN/tailnet), keys protect identity, not transport.

```
arouter:A+   arouter:A   arouter:B
arouter-local:A+   arouter-local:B
arouter-ondemand:A+   arouter-ondemand:A
arouter-night-ondemand:A+   # profile
```

### Integration tests

A black-box Docker rig (`tests/integration/`) drives the real daemon against
stub upstreams: four OpenAI-compatible stubs with runtime-flippable behavior
(`ok · stream · 500 · 429 · 400 · slow`), a fake `aitop` feeding the quota
watcher, and 28 assertions covering the routing matrix — grade ladder and
`A+..A` ranges, pool boundaries, quota-aware skips (with hit counters proving
an exhausted provider is never contacted), failover and error passthrough,
budget 402 breakdowns, cross-pool fallback, SSE streaming, usage/hourly
series, caller attribution, and SIGHUP hot-reload.

```sh
cd tests/integration
./run.sh            # builds arouter-it:local, runs the matrix, tears down
KEEP=1 ./run.sh     # leave the rig up for poking at localhost:3939
```

Watch it live: `arouter monitor` (htop-style TUI — provider LEDs with reset countdowns, per-model quota bars, tok/s EMA, 24h sparklines, spend; `arouter monitor --url host:port` for a remote daemon).

Anthropic-wire clients (pi's `anthropic-messages` API, Claude Code, …) work too — same grades, same pools:

```sh
curl http://127.0.0.1:3929/v1/messages -H 'content-type: application/json' \
  -H "x-api-key: $AROUTER_KEY" \
  -d '{"model":"A","max_tokens":512,"messages":[{"role":"user","content":"hi"}]}'
```

Install & check:

```sh
./install.sh             # release build → ~/.local/bin/arouter → runs `arouter init`
                         # PREFIX=... to target another dir, --skip-init for binary only
arouter doctor      # one-shot health report: daemon, keys, upstreams, quota watcher, pi files
arouter completions bash | sudo tee /etc/bash_completion.d/arouter
```

(`install.sh` is the repo checkout path; `arouter init` alone is enough when the
binary is already installed — it is idempotent and backs up pi's `models.json`
before merging.)

----

<a href="https://www.buymeacoffee.com/engzimmermy" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-yellow.png" alt="Buy Me A Coffee" style="height: 60px !important;width: 217px !important;" ></a>
