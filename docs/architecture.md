# Architecture

arouter is a single-binary daemon that sits between an agent client and
one or more upstream LLM providers. The client asks for a **grade** (or a
routing group); arouter chooses a concrete model using health, quota,
price and policy, then forwards the request on the provider's wire.

## Module layers

The module tree is intentionally acyclic and mirrors the request path:

```text
base      grades, glob, catalog, capabilities      (pure building blocks)
config    TOML schema, validation, key store
routing   registry, discovery                      (static candidates)
state     quota, quotaeng, health, localeng        (dynamic state)
money     budget, usage, tokens                    (spend accounting)
wire      translate, codex, gemini                 (protocol adapters)
serve     proxy, web, metrics, monitor             (HTTP / UI / observability)
ops       init, doctor, overlay                    (CLI and state overlay)
```

`main.rs` is the binary entry point; `lib.rs` is the layer map. Modules
are also re-exported at the crate root (`arouter::quota`,
`arouter::registry`, …) so the historical flat paths remain stable.

## Runtime snapshot

Everything derived from one config load lives in an immutable `Runtime`:

```text
base config + overlay + discovery → Runtime {
    cfg, registry, callers, provider_map, quota sources, overlay diagnostics
}
```

The daemon holds `Arc<RwLock<Arc<Runtime>>>`. A reload builds a complete
new snapshot and swaps the inner `Arc` in one write. A request binds the
snapshot once at entry (`let rt = st.runtime()`), so a request that began
on config N finishes on config N — candidates, caller caps and budget
policy never mix generations. Failed rebuilds keep the previous snapshot
serving.

## Request lifecycle

1. **Route** — `/v1/chat/completions`, `/v1/messages`, or a profile/pool
   path resolves the client-facing `model` (group, group-grade, grade, or
   exact upstream id).
2. **Caller** — a static caller key or an `ark_…` API token establishes
   identity and optional scopes (groups, pools, grades, daily budget).
3. **Translate in** — Anthropic requests become internal chat-completions
   shape; routing never works on two currencies.
4. **Candidate set** — scopes → health → context → capabilities → quota,
   then grade/policy ordering (see [routing.md](routing.md)).
5. **Attempts** — one upstream attempt per candidate with a deadline on
   headers and body, budget reservation for paid pools, health recording,
   and bounded failover. Codex and Gemini adapters plug into the same
   pipeline.
6. **Relay** — success records usage/metrics/token spend exactly once.
   Streaming responses pass through a tap that scans SSE for final usage
   and finalizes on EOF, upstream error, or client cancellation.

## Callers

A caller is identified by, in precedence order:

1. an `ark_…` token from the dynamic token store (scopes apply), or
2. a configured `[[callers]]` key matched from `Authorization: Bearer …`
   / `X-Api-Key`, or
3. in open mode, `X-Arouter-User` and `X-Arouter-Project` headers, or
4. `anonymous`.

`auth.require = true` rejects missing/unknown credentials with 401 and
ignores spoofable identity headers. Observation endpoints (`/status.json`,
`/metrics`, model listings) honor the same auth policy when enabled.

## Metrics

`metrics.rs` maintains request, token, cost and latency counters plus
gauges for models, quota windows, health and in-flight requests. Usage is
recorded per model and per caller/project in `usage.rs` (JSONL audit log
plus a bounded in-memory ring for budget windows). Prometheus renders at
`/metrics`; `/status.json` exposes the same state as JSON.

## Persistence

`server.state_dir` holds:

- `usage.jsonl` — append-only audit trail;
- `tokens.toml` — hashed API tokens (spend is debounced to avoid a
  per-request fsync);
- `web.toml` — operator overlay written by the web UI;
- `discovery.json` / `grades.auto.toml` — persisted discovery state.

All writes are atomic (tmp + fsync + rename) where corruption would lose
credentials or configuration.
