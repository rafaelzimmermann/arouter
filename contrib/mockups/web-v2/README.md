# arouter web console — rework mockups

Design proposal for a console that **configures** arouter instead of merely
displaying API counters.

- **Open the gallery:** [`index.html`](index.html)
- **Product brief and feature list:** [`docs/web-console-rework.md`](../../../docs/web-console-rework.md)
- **Regenerate everything:** `python3 contrib/mockups/web-v2/gen.py`

The SVGs are generated from one Python file, share one palette and can be
diffed. The previous design pass in `contrib/mockups/` is intentionally left
untouched as history.

## Screens

| # | Mockup | What it demonstrates |
|---|---|---|
| 00 | [User flow](00-user-flow.svg) | Setup → observe → diagnose/change → verify; first-run and day-2 paths. |
| 01 | [First access](01-setup.svg) | Admin creation, explicit unclaimed-console warning, onboarding checklist. |
| 02 | [Overview](02-overview.svg) | Health, spend, quota headroom, attention queue, recent changes, verification. |
| 03 | [Models](03-models.svg) | Search/filter, detail, staged enable/weight changes, review and apply. |
| 03a | [Add model](03a-add-model.svg) | Discovery/manual addition, validation, price guard and routing preview. |
| 04 | [Providers & quota](04-providers.svg) | Native balance/windows, staleness, source precedence, override with impact preview. |
| 05 | [Routing](05-routing.svg) | Profiles, pool order, fallback policy and a read-only request simulator. |
| 06 | [API keys](06-keys.svg) | Scope builder, one-time secret, budget, last use and revocation. |
| 07 | [Settings](07-settings.svg) | Config sources, effective config, validate/reload, admin security. |
| 08 | [Mobile](08-mobile.svg) | The same critical flow with thumb-first controls and sticky apply bars. |

## User flow in one picture

```text
FIRST RUN
Open URL → create admin → connect provider → choose models → create key → test

DAY 2
Sign in → overview attention queue → open model/provider → inspect source
      → stage edit → review impact → apply → verify health + sample request
```

Every screen carries a flow rail:

```text
1 Observe  →  2 Diagnose  →  3 Change  →  4 Verify
```

Dashed items in the flow mockup are planned UI. The current daemon already
supports admin setup/login, tokens, model enable/weight/add/remove and quota
overrides; the brief maps every new panel to an existing endpoint or a needed
one.

## Feature highlights

### Overview that makes a decision obvious

- Serving/degraded state, spend, 5xx rate, model health and quota headroom.
- **Needs attention** queue: stale quota, open circuits, unpriced on-demand
  models, low balance, rejected writes.
- Recent changes with actor, time, target and result.
- A verification panel with a ready-to-copy sample request.

### Models with a review step

- Filter by pool, grade, health and source.
- See grade, pools, health, provenance, weight and state.
- Detail drawer shows effective settings, routing stats and affected profiles.
- Enable/disable and weight edits are staged, then reviewed before applying.
- Structure fields (`base_url`, grade, cost) stay config-managed; overlay
  additions remain possible.

### Providers and quota with source precedence

- Native balance or remaining windows, reset time, last fetch and staleness.
- Per-provider routing state: eligible, draining, near limit, excluded.
- Manual override with TTL, audit note and **preview impact**.
- Explicit precedence: fresh native > rate-limit headers > manual override >
  unknown.

### Routing explained, not hidden

- Profiles, pool order, grade floor and fallback policy in one view.
- Candidate simulator distinguishes **excluded** from **ordered**.
- Shows why a model was skipped: pool, token scope, context, capability,
  health or quota.
- Reinforces that fallback never routes upward.

### Keys that are usable by an agent

- Pool/grade/model-glob scope builder.
- Daily budget and expiry.
- “What this key can reach” preview.
- One-time plaintext reveal and a sample `Authorization` header.
- Last-used and spend-today columns.

### Settings with provenance

- Every layer: `web.toml`, `arouter.toml`, discovery, environment,
  `admin.toml`.
- Effective config preview with validate/reload actions.
- Admin account and session security.
- No raw config editor in v1; credentials and structure remain manual.

## Design rules

1. **Attention first** — overview starts with what needs a decision.
2. **Source visible** — manual, discovery, overlay and native are labelled.
3. **Stage, then apply** — no blind writes; show affected profiles first.
4. **Filters are not preferences** — health/quota/scope exclude; weights order.
5. **Secrets once** — plaintext tokens are shown a single time.
6. **No dead ends** — every empty state and alert has a next action.
7. **Mobile is a first-class flow** — not a shrunken table.

## Notes for implementation

- The current web shell has no build step and uses `textContent` only. The
  rework can keep that: render from small typed JSON responses and avoid
  `innerHTML`.
- Staged changes can be implemented entirely on the client with the existing
  `POST /api/models/state`, `POST /api/models/add` and
  `POST /api/quota/override` endpoints.
- The overview, provider detail, routing simulator and audit log need new or
  enriched read endpoints. The brief includes a mapping table and phasing.
- Keep writes same-origin, keep the session cookie `HttpOnly; SameSite=Strict`,
  and keep setup closed after `admin.toml` exists.
