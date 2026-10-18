# Admin setup and login plan

**Status:** implemented — unit, e2e, and manual smoke checks pass
**Scope:** replace the startup claim-link bootstrap with a first-access admin
creation flow and password login.

## Goal

When `[web].enabled = true`:

- the first access to the web UI asks the operator to create the admin
  username and password;
- the credential is persisted and survives daemon restarts;
- later visits ask for username/password instead of a claim link;
- every existing `/api/*` management endpoint keeps requiring an authenticated,
  same-origin session.

## Security decision

**Selected: any first visitor can create the admin.**

While no admin exists, the first request to create credentials wins. This
matches the requested first-access flow, but it means the web port must not be
exposed to untrusted networks until setup has been completed. Startup will
print a warning when no admin exists.

After setup:

- setup is permanently closed (`409`);
- normal username/password login is available from anywhere the web UI is
  reachable;
- repeated failed logins are throttled per peer in memory.

## Design summary

### Credential storage

New file under `server.state_dir`:

```text
<state_dir>/admin.toml
```

Shape:

```toml
version = 1
username = "alice"
password_hash = "$argon2id$v=19$..."
created_unix = 1720000000
updated_unix = 1720000000
```

Rules:

- only an Argon2id hash is stored, never the plaintext password;
- atomic write (`tmp` + `fsync` + rename), mode `0600`;
- missing file means no admin is configured;
- an existing but unreadable/corrupt file fails closed: setup stays unavailable,
  login returns `503`, and the startup log/banner reports the problem;
- no migration is needed: the claim flow never persisted anything.

### Authentication flow

```text
GET /              -> static shell
GET /api/session   -> 401 when unauthenticated
GET /api/setup     -> { required, allowed, locked }
POST /api/setup    -> create first admin + session cookie
POST /api/login    -> verify credentials + session cookie
POST /api/logout   -> clear session cookie
```

Additional rules:

- setup succeeds only while no admin exists;
- setup/login are same-origin writes, matching the current CSRF model;
- bad credentials always return a generic `401`;
- repeated failed logins are throttled in memory per peer;
- sessions remain opaque 32-byte ids in an `HttpOnly; SameSite=Strict` cookie;
- session TTL remains `[web].session_ttl_hours`;
- session data includes the admin username as well as the role.

### Removed behavior

- no `/claim/{token}` route;
- no startup claim token;
- no `web: claim …` banner line;
- no claim-specific tests.

## Task breakdown

### Phase 0 — decide and prepare

- [x] **A0.1 — Confirm setup authorization**
  - Selected: any first visitor may create the admin while none exists.
  - Recorded in the security-decision section above.
  - Acceptance: behavior is unambiguous for implementation and tests.

- [x] **A0.2 — Confirm credential file name and schema**
  - Use `<state_dir>/admin.toml` with the schema above unless there is a
    repository convention that requires a different name.
  - Acceptance: schema includes version, username, password hash, and
    created/updated timestamps.

### Phase 1 — credential store

- [x] **A1.1 — Add the password-hashing dependency**
  - Add `argon2 = "0.5"` to `Cargo.toml`.
  - Update `Cargo.lock`.
  - Acceptance: project builds and Argon2id hashing/verification compiles.

- [x] **A1.2 — Add `src/ops/admin.rs`**
  - Define `AdminRecord` and `AdminStore`.
  - Implement load/save from `<state_dir>/admin.toml`.
  - Implement atomic `0600` writes.
  - Implement Argon2id hash and verify helpers.
  - Acceptance: unit tests can round-trip a record and reject a wrong password.

- [x] **A1.3 — Define validation rules**
  - Username: trimmed, non-empty, bounded length, no control characters.
  - Password: minimum 12 characters, maximum length guard to avoid abuse.
  - Reject empty/whitespace-only values.
  - Acceptance: invalid inputs return a structured `400`, never panic or write a
    partial file.

- [x] **A1.4 — Fail closed on corrupt credentials**
  - Distinguish “file absent” from “file exists but cannot be parsed”.
  - Keep setup disabled and login unavailable (`503`) when the file is corrupt.
  - Log the path and parse error without exposing credentials.
  - Acceptance: corrupt-file test cannot re-run setup or log in.

- [x] **A1.5 — Export the module**
  - Wire `ops::admin` through `src/lib.rs`.
  - Acceptance: `cargo check` passes and the module is usable from `web.rs`.

### Phase 2 — web authentication state

- [x] **A2.1 — Replace claim state in `Web`**
  - Remove `Inner.claim`, `claim_token()`, `consume_claim()`, and `claim_url()`.
  - Add admin state loaded from `AdminStore`.
  - Add `setup_required()`, `setup_locked()`, `admin_username()`, and
    credential create/verify methods.
  - Acceptance: `Web` can represent missing admin, configured admin, and corrupt
    admin-file states.

- [x] **A2.2 — Change construction**
  - Replace `Web::new(&WebConfig)` with a constructor that also receives the
    state directory used to load and persist `admin.toml`.
  - Update all production and test call sites.
  - Acceptance: startup and tests compile with persisted admin loading.

- [x] **A2.3 — Persist the first admin**
  - Implement `create_admin(username, password, now)`.
  - Re-check “no admin exists” under the lock before saving.
  - Hash with Argon2 before taking the write lock where practical.
  - Run hashing/verification in `tokio::task::spawn_blocking`.
  - Acceptance: concurrent setup attempts cannot create two admins.

- [x] **A2.4 — Extend sessions with the username**
  - Add `username` to `Session`.
  - Include it in `/api/session` responses.
  - Keep the existing `role = "admin"` value for compatibility.
  - Acceptance: existing role assertions still pass and the UI can display the
    username.

- [x] **A2.5 — Add login throttling**
  - Track bounded per-peer failure windows in memory.
  - Lock after a small number of failures for a short period.
  - Clear the window on successful login.
  - Return `429` with `Retry-After` while locked.
  - Acceptance: tests cover lockout, expiry, and successful-login reset.

- [x] **A2.6 — Update unauthenticated errors**
  - Replace “claim the daemon…” wording with sign-in/setup wording.
  - Acceptance: no user-facing 401 text references claim links.

### Phase 3 — HTTP routes

- [x] **A3.1 — Remove the claim route**
  - Delete `/claim/{token}` from `src/serve/proxy/routes.rs`.
  - Remove the handler from `src/serve/web.rs`.
  - Acceptance: `/claim/anything` returns `404`.

- [x] **A3.2 — Add `GET /api/setup`**
  - Return public setup state: `{ required, allowed, locked }`.
  - Do not expose the admin username or password hash.
  - Acceptance: missing admin, configured admin, and corrupt-file states return
    the expected values.

- [x] **A3.3 — Add `POST /api/setup`**
  - Parse `{ username, password }`.
  - Enforce same-origin checks.
  - Reject a second setup with `409`.
  - On success: persist, create session, set cookie, return `201`.
  - Acceptance: first setup works once and immediately authenticates the browser.

- [x] **A3.4 — Add `POST /api/login`**
  - Parse `{ username, password }`.
  - Enforce same-origin checks.
  - Return generic `401` for wrong username or password.
  - Return `429` when throttled.
  - On success: mint session and set cookie.
  - Acceptance: correct credentials authenticate; all failure modes remain
    indistinguishable except throttling/availability.

- [x] **A3.5 — Update `/api/session`**
  - Return `authenticated`, `role`, `username`, and `expires_unix`.
  - Acceptance: session probe remains the frontend’s single auth check.

- [x] **A3.6 — Preserve existing authorization**
  - Keep `admin_gate`, same-origin checks, token/model/quota gating, and logout
    behavior unchanged.
  - Acceptance: existing management tests still pass with a minted session.

### Phase 4 — frontend

- [x] **A4.1 — Replace the login section in `web/index.html`**
  - Add a setup form: username, password, confirm password.
  - Add a login form: username, password.
  - Add a corrupt-credential state explaining how to repair `admin.toml`.
  - Acceptance: all forms are usable without inline script and remain compatible
    with the existing CSP.

- [x] **A4.2 — Update `web/app.js` startup flow**
  - Probe `/api/session`.
  - On `401`, probe `/api/setup`.
  - Show setup when required.
  - Show login when an admin already exists.
  - Show repair instructions when the credential file is corrupt.
  - Acceptance: each of the three states renders the correct section.

- [x] **A4.3 — Wire setup and login submissions**
  - POST JSON to `/api/setup` / `/api/login`.
  - Validate password length and confirmation client-side.
  - Display server errors without losing the form.
  - On success, load the overview and management panels.
  - Acceptance: successful setup/login enters the dashboard without a reload
    being required for correctness.

- [x] **A4.4 — Update the header and logout flow**
  - Show the signed-in username and role.
  - Keep logout clearing the cookie and returning to login/setup state.
  - Acceptance: logout then login works end-to-end.

- [x] **A4.5 — Update styles**
  - Add form/input styles consistent with the existing panel styling.
  - Keep the layout usable on narrow screens.
  - Acceptance: setup/login forms match the existing visual language.

### Phase 5 — startup, config, and documentation

- [x] **A5.1 — Update startup wiring**
  - Pass `server.state_dir` into `Web` and load the admin file before the
    server starts.
  - Acceptance: `main.rs` compiles and admin state is available by first request.

- [x] **A5.2 — Replace the startup banner**
  - Remove the claim URL print.
  - Print whether an admin exists.
  - When no admin exists, print the setup URL and warn that the first visitor
    can create the admin.
  - Acceptance: the banner never prints a secret, hash, or password.

- [x] **A5.3 — Update `README.md`**
  - Replace the claim-link instructions with setup/login instructions.
  - Document `admin.toml`, its mode, and the any-first-visitor setup behavior.
  - Document what happens if the file is corrupt.
  - Acceptance: a new user can start the daemon, create an admin, and log in
    using only the README.

- [x] **A5.4 — Update `config.example.toml`**
  - Replace claim-link comments under `[web]`.
  - Keep `enabled` and `session_ttl_hours` documented.
  - Acceptance: no stale claim references remain.

- [x] **A5.5 — Update architecture comments/docs**
  - Update `src/config.rs`, `src/serve/proxy/mod.rs`, and any architecture text
    that describes “sessions/claim”.
  - Acceptance: repository search for claim-link wording finds no stale
    production documentation.

### Phase 6 — tests and validation

- [x] **A6.1 — Add credential-store unit tests**
  - Missing file.
  - Save/load round trip.
  - Unix file mode `0600`.
  - Hash is not the plaintext password.
  - Wrong password rejected.
  - Corrupt file fails closed.

- [x] **A6.2 — Replace web claim unit tests**
  - Setup state transitions.
  - Setup cannot run twice.
  - Login success/failure.
  - Session includes username.
  - Login throttle behavior.
  - Remove `claim_url` and claim-token tests.

- [x] **A6.3 — Update e2e harness**
  - Pass the temp state directory into `Web::new`.
  - Replace the old claim tests with setup/login coverage.
  - Acceptance: harness compiles and existing web tests run.

- [x] **A6.4 — Add e2e setup/login tests**
  - First access reports setup required and allowed.
  - Successful setup sets a session cookie and reaches `/api/overview`.
  - Replayed setup returns `409`.
  - Logout then login succeeds.
  - Wrong password returns `401`.
  - Unauthenticated token/model/quota APIs still return `401`.
  - `[web].enabled = false` hides the new routes too.

- [x] **A6.5 — Run formatting and lint**
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets -- -D warnings`
  - Acceptance: both clean.

- [x] **A6.6 — Run the full test suite**
  - `cargo test`
  - Acceptance: all existing proxy/routing tests plus the new auth tests pass.

- [x] **A6.7 — Manual smoke test**
  - Start with a clean state directory.
  - Verify setup appears and creates `admin.toml` with mode `0600`.
  - Verify restart still requires login and does not re-run setup.
  - Verify corrupting `admin.toml` fails closed.
  - Acceptance: no plaintext password appears in logs, responses, or the file.

### Phase 7 — cleanup and review

- [x] **A7.1 — Remove dead claim code**
  - Search for `claim`, `consume_claim`, `claim_url`, and `claim_token`.
  - Remove or rewrite every stale production reference.
  - Acceptance: no production code or current docs reference the claim flow;
    historical design mockups under `contrib/mockups/` are left untouched.

- [x] **A7.2 — Review the diff for secret leakage**
  - Confirm passwords are never logged, serialized, or placed in URLs.
  - Confirm error responses do not reveal whether the username exists.
  - Acceptance: review sign-off.

- [x] **A7.3 — Record the final decision and notes**
  - Note any deployment guidance discovered during implementation.
  - Add any follow-up work (password change, multiple users, remote setup
    override).
  - Acceptance: this plan is a complete implementation record.

## Expected files

```text
Cargo.toml
Cargo.lock
src/lib.rs
src/ops/admin.rs            (new)
src/serve/web.rs
src/serve/proxy/routes.rs
src/serve/proxy/mod.rs      (comments/state docs)
src/main.rs
src/config.rs               (comments only)
web/index.html
web/app.js
web/style.css
tests/e2e.rs
README.md
config.example.toml
docs/architecture.md       (if it mentions the claim bootstrap)
```

## Validation checklist

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Manual checks:

- [x] `admin.toml` is created with mode `0600`.
- [x] Restart preserves the admin and does not re-open setup.
- [x] Corrupt `admin.toml` fails closed.
- [x] No password or hash appears in logs, API responses, or URLs.
- [x] Existing proxy routes and metrics are untouched.
- [x] Existing token/model/quota management still works after login.

## Implementation notes

- `Web::new(&WebConfig, state_dir)` loads `admin.toml` once at startup.
- `POST /api/setup` returns `201` with a session cookie; replay returns `409`.
- `POST /api/login` returns generic `401` on bad credentials, `429` with
  `Retry-After` while throttled, and `503` if the credential file is unusable.
- `GET /api/setup` is public and returns `{ required, allowed, locked }`;
  `allowed` mirrors `required` under the any-first-visitor policy.
- Tests use an atomic sequence in temporary-directory names so concurrent
  tests cannot share an `admin.toml`.

## Out of scope

- multiple admin users or roles;
- password change or reset UI;
- recovering a lost password without filesystem access;
- TLS termination;
- changes to API-token authentication or routing behavior.
