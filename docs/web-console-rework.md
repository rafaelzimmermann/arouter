# Web console rework

**Status:** design proposal — mockups only, not implemented
**Mockups:** [`contrib/mockups/web-v2/index.html`](../contrib/mockups/web-v2/index.html)
**Generator:** [`contrib/mockups/web-v2/gen.py`](../contrib/mockups/web-v2/gen.py)

## Why rework it

The current shell is a single page: auth form, six counters, a token table, a
model table and a quota form. It exposes the API shape rather than the
operator's job. In practice that means:

- the overview does not say what needs attention;
- providers and quota freshness are invisible;
- a model can be disabled without seeing which profiles are affected;
- a quota override can be written without knowing what routing will do next;
- config-managed and web-managed values look the same;
- the page is desktop-only and mostly tables;
- first access ends at an empty overview instead of a useful next step.

The rework keeps the existing security model and API surface where possible,
but organises the UI around four questions:

1. **Is it healthy?**
2. **What needs a decision?**
3. **What exactly will change?**
4. **How do I verify it?**

## Product principles

| Principle | Consequence |
|---|---|
| Attention first | The overview starts with a short, actionable queue, not all metrics. |
| Source visible | Every value says whether it came from `arouter.toml`, discovery, `web.toml`, `admin.toml`, a native poller or a manual override. |
| Stage, then apply | Edits are pending until reviewed; the review names affected profiles/models and shows a dry-run summary. |
| Filters are not preferences | Routing explanations distinguish excluded (health, quota, scope, pool) from ordered. |
| Secrets once | A generated API key is shown exactly once, with an explicit copy step. |
| No dead ends | Every empty state and alert has the next action. |
| No raw config editor in v1 | Structure and credentials stay manual-only; the UI edits the existing overlay surface and explains how to change the rest. |

## Screen map

| Mockup | Screen | Job |
|---|---|---|
| [`00-user-flow`](../contrib/mockups/web-v2/00-user-flow.svg) | User flow | First-run and day-2 paths, including where the flow is still planned. |
| [`01-setup`](../contrib/mockups/web-v2/01-setup.svg) | First access | Create the admin account and show the remaining setup checklist. |
| [`02-overview`](../contrib/mockups/web-v2/02-overview.svg) | Overview | Health, spend, quota headroom, attention queue, recent changes and verification. |
| [`03-models`](../contrib/mockups/web-v2/03-models.svg) | Models | Search/filter, inspect, stage enable/weight changes, review impact, apply. |
| [`03a-add-model`](../contrib/mockups/web-v2/03a-add-model.svg) | Add model | Discovery/manual addition, validation, price guard and routing preview. |
| [`04-providers`](../contrib/mockups/web-v2/04-providers.svg) | Providers & quota | Native balance/windows, staleness, manual override and routing impact preview. |
| [`05-routing`](../contrib/mockups/web-v2/05-routing.svg) | Routing | Profiles, pool order, fallback policy and a read-only request simulator. |
| [`06-keys`](../contrib/mockups/web-v2/06-keys.svg) | API keys | Scoped keys, one-time secret, budget, last use and revocation. |
| [`07-settings`](../contrib/mockups/web-v2/07-settings.svg) | Settings | Config sources, effective config, validation/reload and admin security. |
| [`08-mobile`](../contrib/mockups/web-v2/08-mobile.svg) | Mobile | The same flow with thumb-first controls and sticky apply bars. |

## Feature list

### P0 — make the current surface usable

- **Attention queue** — stale quota, open circuits, unpriced on-demand models,
  low balance, failed writes and config errors, each with a resolution link.
- **Provider cards** — remaining quota or absolute balance, reset time,
  source, age, availability and routing state for every provider.
- **Provider detail** — windows, balance, source precedence, manual override
  with TTL and a preview of affected models.
- **Model detail** — grade, pools, source, weight, health, latency, fail ratio,
  affected profiles and the difference between structure and operational fields.
- **Add-model wizard** — discovery or manual endpoint, auth, grade/pool/context,
  on-demand cost guard, validation and a candidate preview before applying.
- **Staged model changes** — selection, enable/disable and weight edits are
  pending until the operator reviews and applies them.
- **Impact preview** — before apply, show affected profiles, candidate count,
  exclusions and the dry-run result.
- **Source badges** — `config`, `discovery`, `overlay`, `manual`, `native` are
  always visible.
- **First-run checklist** — after admin creation: connect provider, choose
  models, create key, send test request.
- **Better API-key flow** — scope builder, budget, expiry, last use, one-time
  reveal and a generated sample request.

### P1 — explain and verify

- **Routing simulator** — choose profile, pool, grade and context; show the
  candidate order and the reason for every skip.
- **Recent changes / audit** — who changed what, when, source and result.
- **Effective config view** — read-only preview with layer provenance and a
  copy/download action.
- **Validate / reload** — validate the merged config before rebuilding.
- **Verification panel** — health, error rate, spend and a sample call after a
  change.
- **Mobile layout** — attention queue, model switches and quota overrides.
- **Empty states** — no providers, no models, no keys, no traffic each get a
  next action.

### P2 — future management surface

- Users, roles and access rules.
- Provider onboarding from the UI.
- Password change and session management.
- Config editing for safe fields (never raw credentials).
- Audit export and scheduled reports.
- Per-caller usage detail.

## User flows

### First run

```text
Open URL
  → no admin: create admin (Argon2id, 0600)
  → session starts
  → checklist: connect provider → choose models → create first key
  → send a test request
  → overview shows traffic and closes setup
```

The first visitor can claim an unconfigured console. The setup screen must say
this explicitly and recommend completing setup before exposing the port.

### Day-2 change

```text
Sign in
  → overview attention queue
  → open the affected model/provider
  → inspect source, health and current routing state
  → stage an edit
  → review: affected profiles, dry-run, exclusions
  → apply (same-origin write)
  → verify: health, error rate, sample request
```

### Add a model

```text
Models → Add model
  → choose discovery result or manual endpoint
  → validate id/base_url/grade/pool
  → ondemand requires cost_in + cost_out (unpriced is refused)
  → test connection (planned)
  → review and apply to web.toml overlay
  → verify it appears in the intended profiles
```

Structure fields on an existing model remain manual-only. The console can add
an overlay model, enable/disable and set weight; it must not silently rewrite
`arouter.toml`.

### Fix stale quota

```text
Overview: “z.ai quota stale 18m”
  → provider detail: windows, age and source
  → set a manual window + TTL
  → preview routing impact
  → apply
  → verify provider is no longer draining
```

A fresher native sample supersedes the manual override. The UI must show that
precedence instead of implying the override is permanent.

### Create a key

```text
API keys → Create key
  → name, pools, grades, model glob, daily budget, expiry
  → preview what the key can reach
  → create
  → copy the plaintext once
  → show the sample Authorization header
```

Scopes only narrow access. The key can never exceed the caller's pool, grade or
budget.

## Information architecture

```text
Operate
  Overview          attention, health, spend, traffic, recent changes
  Models            catalog, health, weight, enable/disable, add/remove overlay
  Providers & quota native balance/windows, staleness, overrides
  Routing           profiles, pool order, fallback, simulator

Access
  API keys          scoped keys, budget, last use, revoke
  Users             planned

System
  Settings          config sources, effective config, validation, admin
```

## Current capability vs mockup

| Mockup area | Current API / state | Work needed |
|---|---|---|
| Admin setup/login | `GET /api/setup`, `POST /api/setup`, `POST /api/login`, `GET /api/session` | UI only |
| Overview counters | `GET /api/overview` | Enrich response with spend, errors, health counts, attention |
| Models list | `GET /api/models` | Add health, latency, fail ratio, effective cost, affected profiles |
| Model enable/weight | `POST /api/models/state` | Client-side staging + review; optional batch endpoint |
| Add/remove overlay model | `POST /api/models/add`, `/remove` | UI wizard and validation preview |
| Quota overrides | `POST /api/quota/override`, `GET /api/models` | Provider detail endpoint, impact preview, source precedence |
| Tokens | `GET/POST /api/tokens`, `/api/tokens/revoke` | Add last-used, expiry, budget usage |
| Providers | `GET /status.json` (quota providers) | Dedicated provider view with native balance and freshness |
| Routing simulator | `/status.json` + routing docs | New read-only endpoint |
| Audit / recent changes | none | Append-only activity log |
| Effective config / validate / reload | config load only | New admin endpoints, strict redaction |
| Mobile | static CSS | Responsive layout |

## Implementation phases

1. **Shell + overview** — navigation, attention queue, provider/model counts,
   first-run checklist. Reuse existing endpoints; add enrichment where cheap.
2. **Models** — detail drawer, filters, staged changes, review/apply. This makes
   the existing overlay genuinely usable.
3. **Providers & quota** — cards, detail, source precedence, override preview.
4. **API keys** — scope builder, one-time secret, sample request.
5. **Routing** — profile explanation and read-only simulator.
6. **Settings** — config source view, validate/reload, admin security.
7. **Mobile + accessibility** — collapsible nav, card tables, sticky apply bar,
   keyboard navigation and focus states.

## Acceptance criteria

- An operator can answer “what needs attention?” without opening Grafana.
- No write happens without a review step that names the target and impact.
- Every displayed value has a visible source.
- A stale provider can be diagnosed and overridden without editing TOML.
- A bad merge is rejected before it replaces the running snapshot.
- A new key can be created, copied once and test-called from the UI.
- The first-run path ends with a verified request, not an empty dashboard.
- The same critical flows work at phone width.
