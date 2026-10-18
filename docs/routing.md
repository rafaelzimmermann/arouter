# Routing

Routing is deterministic given the config and the current runtime state.
The client asks for a grade; arouter never silently routes upward.

## §1 Candidate set

The candidate set for `(profile, pool, grade request)` is built in this
order. Every filter removes a model from consideration; it is never merely
deprioritized:

1. **Visibility** — pool membership, model profile allow-list, profile
   include/exclude globs, routing-group membership.
2. **Token scopes** — a token's group/pool/grade allow-lists intersect
   the set before any ordering, so an out-of-scope upstream is never
   contacted.
3. **Health** — models in a circuit-breaker cooldown are excluded.
4. **Context** — a model whose declared context window cannot hold the
   estimated input plus requested output is excluded.
5. **Capabilities** — a request with an image part requires `vision`; a
   request with `reasoning_effort` requires `reasoning`. An explicit
   exact-model pin bypasses capability filtering (the operator asked for
   that model).
6. **Quota** — a provider with no remaining quota is excluded, except in
   the `local` pool (local engines do not consume provider quota).

The pre-quota list is retained so an exhaustion response can quote a
reset-aware `Retry-After`.

### Capabilities

Capabilities are resolved per model from explicit
`[[models]].capabilities` (authoritative when non-empty), the curated
`src/base/capabilities.toml` table, and a narrow name heuristic.
Capability names are `vision` and `reasoning`.

## §2 Grades, ladders, and fallback

A request names a grade token (`A+`, `A`, `B`, …) or a range such as
`A+..A`. A ladder orders tokens best-first. Routing groups may define
their own ladder and per-member grade overrides.

The default fallback policy allows the requested grade and one grade
below it. `routing.fallback.allow` can widen or narrow that per requested
grade. Fallback **never routes upward**: entries above the requested grade
are rejected in validation and filtered at selection.

Cross-pool fallback is explicit opt-in:

- a routing group's `fallback = [...]` list;
- `[routing.fallback] cross_pool = [...]` for the legacy grade surface;
- a profile overlay may replace the list (`cross_pool = []` explicitly
  disables inherited paid fallback).

Cross-pool targets still pass their own budget preflight.

## §3 Health and quota availability

**Health** is a per-model circuit breaker: failure ratio over the recent
window opens the breaker; cooldowns grow 30s → 2m → 8m → 30m; a success
closes it. Failures and successes are recorded on every termination path,
including stream errors and client aborts.

**Quota** state is per provider/panel. Windows carry a used percentage and
optional reset time. Staleness is per provider; the newest signal wins.
Routing treats an exhausted or draining window as unavailable, and prefers
models with quota headroom. HTTP 429 responses and rate-limit headers are
ground truth even when a quota API says otherwise. Availability uses a
shortage score from the worst window plus pace/reset information.

## §4 Preference

Within a grade, ordering is governed by preference:

- `cheapest` — lowest blended cost first (default for `ondemand`);
- `fastest` — lowest observed latency first;
- `manual` — configured weight first (default for `quota` and `local`).

Preference resolves once per request: profile override → pool config →
pool default. The value is not reinterpreted during failover.

## §5 Attempt ordering

For the legacy grade surface, candidates are ordered by grade (best
first), then preference, then configured weight, then id. For a routing
group, ordering uses the group's ladder and membership. Local models get a
residency boost when their engine reports them loaded, and the `local`
pool adds warm-cache stickiness per caller/grade.

## §6 Budget guards

The `ondemand` pool is chargeable and guarded by stacked caps:

1. per-request estimate;
2. caller daily cap (config or token);
3. profile pool caps;
4. pool daily/monthly/pace caps.

Admission is atomic: check-and-reserve under one lock, so concurrent
requests cannot both pass a cap. A successful attempt settles the hold;
a failed attempt drops it. A refusal is a 402 with a machine-readable
breakdown naming the tightest guard. Unpriced models are refused in the
paid pool.

## §7 Quota ingestion

Quota signals arrive from:

- native pollers (`native:openrouter`, `native:deepseek`, `native:zai`) for
  key/admin limits and absolute account balances;
- rate-limit headers and 429 `Retry-After` harvested from real traffic;
- the legacy external command, or manual windows from the web overlay.

DeepSeek's balance endpoint reports an absolute per-currency balance. It is
exported as `arouter_provider_balance{provider,currency}` (plus granted,
topped-up, availability, and fetch timestamp gauges); a non-spendable account
also produces a synthetic 100% `balance` window so routing excludes it until
the account is topped up. An explicit `quota.sources."deepseek"` or a
manual window for that prefix takes precedence and disables the native
poller.

OpenRouter wallet credits are not visible to the key-limit API. A 402 with
`error.metadata.limit_source = "openrouter_credits"` records a short
provider-level exhausted window; a later successful inference clears it.
This prevents one request from trying several models against an empty
wallet.
