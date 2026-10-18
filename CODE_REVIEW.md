# Code Review — arouter

Review scope: code quality, code organization, architecture.
Reviewed: `src/` (all modules), `tests/`, CI, and project layout.

---

## 1. Overall verdict

Strong architecture for a quota-aware LLM router. The hard parts —
atomic hot-reload, concurrent budget admission, exactly-once usage
accounting, and bounded failover — are done correctly and consistently.
The issues found are mostly "finishing" defects rather than structural
flaws, plus one in-flight feature that currently breaks the test build.

---

## 2. Architecture — strengths

- **Layered, single-responsibility modules.**
  Config/validation (`config`, `grades`, `glob`) → static routing
  (`registry`) → dynamic scoring (`quota`, `quotaeng`, `health`,
  `localeng`, `catalog`, `discover`, `capabilities`) → budget
  (`budget`, `usage`, `tokens`) → wire adapters (`translate`, `codex`,
  `gemini`) → serving (`proxy`, `web`, `metrics`, `monitor`).
- **Immutable runtime snapshot hot-reload** (`Arc<RwLock<Arc<Runtime>>>`).
  Registry + callers + budgets + policy swap atomically; a request that
  started on config N finishes on config N. Correctly documented as
  "review F6".
- **RAII budget holds.** `budget::ReserveGuard` + `usage::admit` do
  check-and-reserve under one lock, so concurrent requests cannot both
  pass a cap. Settled exactly once via `settle()` / `Drop`.
- **Exactly-once stream finalization.** `proxy::TapStream` uses a
  `recorded` flag + `Drop` (client abort) + explicit `finish()`, so
  usage/health/metrics are recorded once on any termination path.
- **Deadline-bounded failover** everywhere: headers/body/SSE-gap
  deadlines, `max_attempts` cap.
- **Adapter isolation.** Codex and Gemini translation live in their own
  modules and plug into the same routing/budget/health pipeline.
- **Good test layering + CI.** Inline unit tests, `tests/e2e.rs`, and a
  black-box Docker rig (`tests/integration/`) under a CI matrix
  (fmt + `clippy -D warnings` + tests + rig).

---

## 3. Code organization — issues

1. **`src/proxy.rs` is a ~3,000-line monolith.** It holds the axum
   router, thin handlers, caller resolution, `handle_chat` /
   `try_candidates`, three wire-attempt functions, `relay_success`, three
   SSE adapter stream types, the usage tap, and helpers. Suggest splitting
   into e.g. `routes.rs`, `attempt.rs`, `sse.rs`, `tap.rs`. The adapter
   attempt functions (`codex_attempt`, `gemini_attempt`) belong in
   `codex.rs` / `gemini.rs`.

2. **`handle_chat` is ~400 lines with a large nested `build_candidates`
   closure.** The scope → health → context → capability → quota filter
   chain is inlined. Extract a `CandidateSet` builder for readability and
   testability of the exclusion diagnostics.

3. **Design docs referenced but absent.** `src/lib.rs` says "See PLAN.md
   and docs/", and code cites `docs/routing.md` (13×), `PLAN.md` (6×),
   `docs/grading.md` (5×), `docs/architecture.md`, `docs/profiles.md`.
   None exist in the repo — only `README.md` and
   `PRODUCTION_DEPLOYMENT.md`. Either commit the docs or repoint the
   references.

4. **`POOLS` is duplicated.** `["quota","local","ondemand"]` is hardcoded
   in `config.rs::POOLS`, `proxy.rs`, and `monitor.rs::POOL_KEYS`; pool
   strings are also compared inline throughout. Use one enum/const.

5. **Stringly-typed domains.** `m.wire == "codex"`, `pool == "ondemand"`,
   preference `"cheapest" | "fastest" | "manual"` are bare strings.
   `ModelConfig::wire_kind()` normalizes but `Model::wire` is still a
   `String`. Enums would make these exhaustive-matchable.

6. **Mixed error types.** Some paths return `Result<_, String>`
   (`Runtime::build`, `config::validate`), others `anyhow::Result`.
   Pragmatic, but inconsistent context propagation.

7. **Flat, unlayered module layout — no packages, no visual grouping.**
   All 23 modules sit flat in `src/` as siblings declared alphabetically
   in `lib.rs`. There is no Cargo workspace and no directory structure,
   so the filesystem gives no hint of which files belong together or
   where the entry points are. The layering is real and acyclic
   (`config` → `registry`/`discover` → `quota`/`health`/`localeng` →
   `budget`/`usage`/`tokens` → `translate`/`codex`/`gemini` → `proxy` →
   CLI/ops) but is only implied by naming suffixes (`quota`/`quotaeng`,
   `localeng`). Entry points are `main.rs` (binary CLI → `serve()`) and
   `lib.rs` (the module map).

   *Recommended:* nest modules into directories that mirror the layers
   so the tree is self-documenting and `lib.rs` becomes a layer map
   instead of an alphabetical list:

   ```text
   src/
     main.rs                  # binary entry point
     lib.rs                   # layered module map
     base/                    # grades, glob, catalog, capabilities
     config.rs
     routing/                 # registry, discover
     state/                   # quota, quotaeng, health, localeng
     money/                   # budget, usage, tokens
     wire/                    # translate, codex, gemini
     serve/                   # proxy, web, metrics, monitor
     ops/                     # init, doctor, overlay
   ```

   A Cargo workspace with real crates is deferred until something must be
   consumed externally — it would force crate-internal `pub` items into a
   public API surface for little gain in a single-binary daemon.

---

## 4. Concrete defects

Status update (2026-10-17): the capability fixtures in §4.2 are already
complete in the reviewed checkout. This pass also fixed the Gemini duplicate
and OpenRouter wallet-credit handling described below.

### 4.1 Duplicate, unreachable Gemini dispatch — `src/proxy.rs`

The Gemini dispatch block had been copy-pasted twice:

```rust
// line ~1319
if m.wire == "gemini" {
    match gemini_attempt(...) {
        Ok(r) => return Ok(r),
        Err(e) => { ...; continue; }   // always returns or continues
    }
}
// line ~1348 — identical, unreachable
if m.wire == "gemini" { ... }
```

The first block always `return`s or `continue`s, so the second was dead
code. The duplicate dispatch has been removed.

### 4.2 Capability test fixtures — resolved in current checkout

The `capabilities` field was added to `config::ModelConfig` and
`registry::Model`. The cited `src/quota.rs` fixtures now initialize it:

```
error[E0063]: missing field `capabilities` in initializer of `registry::Model`
  --> src/quota.rs:1228, 1315, 1358, 1433
```

The four locations cited in the original review now include
`capabilities: Default::default()`. The former build failure is stale; this
review pass did not run the test suite.

### 4.5 OpenRouter wallet credits were not checked before failover

OpenRouter returns HTTP 402 with
`error.metadata.limit_source = "openrouter_credits"` when account credits
are exhausted. The proxy previously treated every 402 as a normal
model-specific failover, so one request could try several models against the
same empty account. The OpenRouter key quota poll reports key limits and
cannot establish wallet balance.

The proxy now records a five-minute provider-level exhausted window for this
specific machine-readable 402. It stores the block under the configured quota
panel mapping (the live config maps `or` to `openrouter`) so candidate
filtering sees the same state. Candidate availability is rechecked during
failover, so remaining models for that provider are skipped. Other 402s
retain their existing failover behavior. A successful OpenRouter inference
clears the temporary wallet block; native key-limit polls preserve it until
then or until its cooldown expires. A focused regression test covers the
explicit provider-map case.

### 4.3 Token-store write amplification

`TokenStore::charge_by_caller` is called on every stream finish and
on-demand relay, and each call triggers `save()` — full TOML
serialization + `fsync` + atomic rename of the whole token file. At high
request rates this is a per-request `fsync`. Debounce or batch token-spend
persistence.

### 4.4 Hot lock in the usage path

`UsageLog::record` holds the global `Mutex<Inner>` while doing
unbuffered file I/O (`writeln!`). All records serialize through this one
lock. Acceptable at current scale, but it is the hottest lock on the
request path.

---

## 5. Observability — dashboard and metrics gaps

The Grafana dashboard (`contrib/grafana-dashboard.json`) now includes a
DeepSeek row, caller/pool/provider/model/quota-provider filters,
provider-level traffic and spend, error-rate/TTFB/status panels, model
health/cooldown/failure panels, a quota-provider mapping table, and an
explicit failover view. Findings from the first pass are addressed:

1. **Latency histograms have no model/provider labels.** ✅ Addressed:
   `arouter_request_duration_seconds` and `arouter_stream_ttfb_seconds`
   now carry bounded `model` and `provider` labels, so latency/TTFB can be
   filtered and graphed per model (including DeepSeek).
2. **Health/cooldown state is not exported as metrics.** ✅ Addressed:
   `/metrics` exports `arouter_model_health` (0 healthy, 1 cooldown,
   2 circuit open), `arouter_model_cooldown_secs`, and
   `arouter_model_fail_ratio_1h`, computed from the same health snapshot
   as `/status.json`.
3. **Provider is reconstructed in PromQL.** ✅ Addressed: request, token,
   cost, latency, and TTFB metrics now have a native `provider` label
   derived from the model-id prefix. Provider dashboards and filters no
   longer use `label_replace`.
4. **Quota and usage provider namespaces differ.** ✅ Addressed:
   `arouter_quota_provider_info{provider,quota_provider}` exports the
   resolved `provider_map` join, and the dashboard includes a mapping
   table. The Model and Quota provider filters remain separate because
   one selects registry prefixes and the other selects aitop panel names.
5. **No alert or recording rules.** ✅ Addressed:
   `contrib/observability/prometheus/rules/arouter.yml` adds recording
   rules for error ratio and spend velocity/window, plus alerts for target
   down, high 5xx rate, stale/exhausted quotas, unavailable balance, open
   circuits, sustained degradation, and high spend. Thresholds are
   conservative starting points and should be tuned per deployment. The
   live and integration Prometheus configs mount the same rules directory.
6. **DeepSeek account balance was not collected.** ✅ Addressed:
   `native:deepseek` polls the documented `/user/balance` endpoint and
   stores per-currency total/granted/topped-up amounts on the provider
   state. `/metrics` exports `arouter_provider_balance*` gauges,
   `/status.json` exposes the same snapshot, and a non-spendable account
   produces a synthetic 100% `balance` window so routing excludes it until
   topped up. The dashboard adds availability, freshness, and
   balance-by-currency panels.

## 6. Recommended order

1. **Fixed:** complete the capability fixture migration, remove the dead
   duplicate Gemini dispatch, and suppress repeated OpenRouter wallet-credit
   failures.
2. **Then:** restructure into nested module directories (see §3.7) so
   `lib.rs` is a layer map and related files are visibly grouped.
3. **Then:** split `proxy.rs`; extract the candidate builder; commit (or
   stop referencing) the missing `docs/` / `PLAN.md`; de-duplicate
   `POOLS`; replace stringly-typed wire/pool/preference with enums.
4. **Perf:** debounce token-store `save()`; revisit the usage-log mutex
   scope.
5. **Observability:** ✅ model/provider labels, health/cooldown gauges,
   DeepSeek latency/error and account-balance panels, quota-provider
   mapping, and alert rules are in place; tune alert thresholds against
   real traffic.
