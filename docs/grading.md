# Grading and discovery

Grades are labels (`A+`, `A`, `B`, …) defined by `[grades] ladder` in the
config. Manual `[[models]]` entries state their grade explicitly.

## Catalog

`src/base/catalog.toml` is the curated table of known model families and
their default grades. It is used for models that have no manual grade and
no operator override. It is embedded in the binary and validated at
startup; an invalid embedded table is a startup error.

## Heuristic

For models absent from the catalog, a conservative name heuristic guesses
a grade from family/size tokens. Heuristic grades are marked with the
`heuristic` provenance so operators can audit or override them.

## Discovery

Upstream discovery polls configured model endpoints (`/v1/models`, and
ollama's `/api/tags`) for models that are not in the config. Discovered
models inherit pool/auth/visibility from a sibling model on the same
upstream, then get a grade through this precedence chain:

1. `grades.auto.toml` (operator-approved automatic grades);
2. the embedded catalog;
3. the name heuristic.

Discovery is controlled by `routing.discovery_interval_secs`. A value of
`0` disables discovery entirely — including the startup/SIGHUP pass. The
discovered set is merged with manual config; manual entries always win.

## Provenance

Every model reports where its grade came from: `manual`, `auto`,
`catalog`, or `heuristic`. Provenance appears in `/status.json`, the web
UI, and `/v1/models`-adjacent diagnostics, and is never used to change
routing semantics.
