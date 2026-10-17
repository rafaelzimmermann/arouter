# Profiles

Profiles are named policy overlays over the base routing config. They are
selected by the URL path:

```text
/v1/chat/completions              base (default profile)
/{profile}/v1/chat/completions    profile
/{profile}/{pool}/v1/chat/completions
```

`default`, `local`, and `ondemand` are reserved names; the pool paths are
their own surface (`/local/...`, `/ondemand/...`).

## What a profile can change

A `[profiles.<name>]` section may set:

- a description (shown in status/web);
- a visibility allow/deny list (`models.include` / `models.exclude`,
  glob patterns over registry ids);
- a preference override (`cheapest` / `fastest` / `manual`);
- a fallback overlay:
  - `fallback.allow` merges per requested grade;
  - `fallback.cross_pool` replaces the inherited list when present
    (`[]` explicitly disables paid fallback);
- budget caps for the profile.

Model-side visibility (`[[models]].profiles`) and profile-side
include/exclude are intersected: both must allow the model.

## Precedence

For any request the effective policy is:

```text
model profile allow-list
  ∩ profile include/exclude
  ∩ token scopes
  ∩ pool boundary
```

Preference resolves profile override → pool config → pool default.
Fallback resolves profile overlay → base routing config. Budget caps stack
and the tightest one wins.

Profiles never change wire translation, quota ingestion, or health
semantics; they only narrow or reorder what the base router may choose.
