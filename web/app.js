// arouter web shell (E10a). No dependencies, no build step, no inline
// script: everything here is fetched from the daemon and rendered with
// textContent (never innerHTML) so upstream strings can't inject markup.
"use strict";

const $ = (id) => document.getElementById(id);

async function api(path, init) {
  const resp = await fetch(path, { credentials: "same-origin", ...init });
  const body = await resp.json().catch(() => ({}));
  return { status: resp.status, body };
}

function card(key, value, cls) {
  const el = document.createElement("div");
  el.className = "card";
  const k = document.createElement("div");
  k.className = "k";
  k.textContent = key;
  const v = document.createElement("div");
  v.className = "v" + (cls ? " " + cls : "");
  v.textContent = value;
  el.append(k, v);
  return el;
}

function renderOverview(o) {
  const stats = $("stats");
  stats.replaceChildren(
    card("models", String(o.models)),
    card("profiles", String(o.profiles.length), ""),
    card("callers", String(o.callers)),
    card("auth", o.auth_required ? "required" : "open"),
    card("uptime", o.uptime_secs + "s"),
    card("usage sink", o.sink_degraded ? "degraded" : "ok", o.sink_degraded ? "bad" : "good")
  );
  const panel = $("panel");
  panel.hidden = false;
  $("login").hidden = true;
}

// E10b — token management. csv fields become arrays; empty stays empty.
const csv = (v) => v.trim() ? v.trim().split(/\s*,\s*/) : undefined;

function scopeSummary(sc) {
  const bits = [];
  if (sc.groups) bits.push("groups=" + sc.groups.join(","));
  if (sc.pools) bits.push("pools=" + sc.pools.join(","));
  if (sc.grades) bits.push("grades=" + sc.grades.join(","));
  if (sc.daily_budget_usd != null) bits.push("≤$" + sc.daily_budget_usd + "/day");
  return bits.length ? bits.join(" · ") : "unscoped";
}

async function renderTokens() {
  const t = await api("/api/tokens");
  if (t.status !== 200) return;
  const rows = $("tokens").querySelector("tbody");
  rows.replaceChildren();
  for (const tok of t.body.tokens) {
    if (tok.revoked) continue;
    const tr = document.createElement("tr");
    for (const text of [
      tok.name + " · " + tok.id,
      scopeSummary(tok.scopes),
      "$" + tok.spent_today_usd.toFixed(4),
    ]) {
      const td = document.createElement("td");
      td.textContent = text;
      tr.append(td);
    }
    const td = document.createElement("td");
    const btn = document.createElement("button");
    btn.textContent = "revoke";
    btn.addEventListener("click", async () => {
      await api("/api/tokens/revoke", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: tok.id }),
      });
      renderTokens();
    });
    td.append(btn);
    tr.append(td);
    rows.append(tr);
  }
}

$("mktoken").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const budget = parseFloat($("tk-budget").value);
  const r = await api("/api/tokens", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      name: $("tk-name").value.trim(),
      groups: csv($("tk-groups").value),
      pools: csv($("tk-pools").value),
      grades: csv($("tk-grades").value),
      daily_budget_usd: isNaN(budget) ? undefined : budget,
    }),
  });
  if (r.status === 201) {
    const once = $("tk-once");
    once.hidden = false;
    once.textContent = r.body.token + "   ← copy now, shown once";
    $("mktoken").reset();
    renderTokens();
  } else {
    const once = $("tk-once");
    once.hidden = false;
    once.textContent = "error: " + (r.body.error || r.status);
  }
});

// E10c — model + quota management. Writes are same-origin (the shell
// is served by the daemon itself, so fetch() sends Origin/Host).

async function renderModels() {
  const r = await api("/api/models");
  if (r.status !== 200) return;
  const body = r.body;
  const errs = [].concat(body.overlay.errors, body.overlay.load_errors)
    .filter(Boolean);
  const errEl = $("ov-errors");
  if (errs.length) {
    errEl.hidden = false;
    errEl.textContent = "overlay: " + errs.join(" · ");
  } else {
    errEl.hidden = true;
  }
  const rows = $("models").querySelector("tbody");
  rows.replaceChildren();
  const now = Date.now() / 1000;
  for (const m of body.models) {
    const tr = document.createElement("tr");
    if (m.disabled) tr.className = "off";
    const cells = [
      m.id,
      m.grade || "—",
      m.pools.join(","),
      m.provenance,
      String(m.weight),
    ];
    for (const text of cells) {
      const td = document.createElement("td");
      td.textContent = text;
      tr.append(td);
    }
    const td = document.createElement("td");
    td.className = "acts";
    if (m.provenance === "overlay" || m.disabled) {
      // only overlay additions (and born-disabled additions) can go away
      if (m.provenance !== "hidden" || m.disabled) {
        const rm = document.createElement("button");
        rm.textContent = "remove";
        rm.addEventListener("click", async () => {
          const r2 = await api("/api/models/remove", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ id: m.id }),
          });
          if (r2.status !== 200) alert(r2.body.error || r2.status);
          renderModels();
        });
        td.append(rm);
      }
    }
    const tog = document.createElement("button");
    tog.textContent = m.disabled ? "enable" : "disable";
    tog.addEventListener("click", async () => {
      const r2 = await api("/api/models/state", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: m.id, disabled: !m.disabled }),
      });
      if (r2.status !== 200) alert(r2.body.error || r2.status);
      renderModels();
    });
    td.append(tog);
    const wt = document.createElement("button");
    wt.textContent = "weight";
    wt.addEventListener("click", async () => {
      const v = parseFloat(prompt("weight for " + m.id + " (> 0)", String(m.weight)));
      if (isNaN(v)) return;
      const r2 = await api("/api/models/state", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ id: m.id, weight: v }),
      });
      if (r2.status !== 200) alert(r2.body.error || r2.status);
      renderModels();
    });
    td.append(wt);
    tr.append(td);
    rows.append(tr);
  }
  const qrows = $("qov").querySelector("tbody");
  qrows.replaceChildren();
  for (const o of body.quota_overrides) {
    const tr = document.createElement("tr");
    if (!o.active) tr.className = "off";
    const reset = o.reset_unix
      ? Math.max(0, Math.round(o.reset_unix - now)) + "s"
      : "—";
    const ttl = Math.max(0, Math.round(o.expires_at - now)) + "s";
    for (const text of [
      o.provider,
      o.label,
      Math.round(o.pct * 100) + "%",
      reset,
      o.active ? ttl : "expired",
    ]) {
      const td = document.createElement("td");
      td.textContent = text;
      tr.append(td);
    }
    qrows.append(tr);
  }
}

$("addmodel").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const r = await api("/api/models/add", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      id: $("am-id").value.trim(),
      base_url: $("am-url").value.trim(),
      grade: $("am-grade").value.trim(),
      pool: csv($("am-pool").value),
      auth: $("am-auth").value.trim() || undefined,
    }),
  });
  if (r.status !== 200) alert(r.body.error || r.status);
  $("addmodel").reset();
  renderModels();
});

$("qovform").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const pct = parseFloat($("qo-pct").value);
  const reset = parseInt($("qo-reset").value, 10);
  const ttl = parseInt($("qo-ttl").value, 10);
  const r = await api("/api/quota/override", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      provider: $("qo-provider").value.trim(),
      label: $("qo-label").value.trim(),
      pct: pct,
      reset_in_secs: isNaN(reset) ? undefined : reset,
      ttl_secs: isNaN(ttl) ? undefined : ttl,
    }),
  });
  if (r.status !== 200) alert(r.body.error || r.status);
  $("qovform").reset();
  renderModels();
});

function authError(id, message) {
  const el = $(id);
  if (!message) {
    el.hidden = true;
    el.textContent = "";
    return;
  }
  el.hidden = false;
  el.textContent = "error: " + message;
}

function showAuth(mode, note) {
  $("panel").hidden = true;
  $("who").textContent = "";
  $("login").hidden = false;
  $("setup-form").hidden = true;
  $("login-form").hidden = true;
  $("auth-locked").hidden = true;
  authError("setup-error");
  authError("login-error");
  $("auth-note").hidden = !note;
  $("auth-note").textContent = note || "";
  if (mode === "setup") {
    $("auth-title").textContent = "create admin";
    $("setup-form").hidden = false;
    $("setup-user").focus();
  } else if (mode === "login") {
    $("auth-title").textContent = "sign in";
    $("login-form").hidden = false;
    $("login-user").focus();
  } else {
    $("auth-title").textContent = "admin unavailable";
    $("auth-locked").hidden = false;
    $("auth-locked").textContent =
      "The credential file exists but cannot be read. Repair or remove " +
      "admin.toml in the state directory, then restart arouter.";
  }
}

async function refresh() {
  $("host").textContent = location.host;
  const s = await api("/api/session");
  if (s.status !== 200) {
    const setup = await api("/api/setup");
    if (setup.status === 200 && setup.body.locked) {
      showAuth("locked");
    } else if (setup.status === 200 && setup.body.required) {
      showAuth("setup", "No admin exists yet.");
    } else {
      showAuth("login", "Sign in with the admin account.");
    }
    return;
  }
  $("who").textContent = s.body.username + " · " + s.body.role +
    " · expires in " +
    Math.max(0, Math.round((s.body.expires_unix - Date.now() / 1000) / 60)) + "m";
  const o = await api("/api/overview");
  if (o.status === 200) renderOverview(o.body);
  renderTokens();
  renderModels();
}

$("setup-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  authError("setup-error");
  const username = $("setup-user").value.trim();
  const password = $("setup-pass").value;
  const confirm = $("setup-pass2").value;
  if (password.length < 12) {
    authError("setup-error", "password must be at least 12 characters");
    return;
  }
  if (password !== confirm) {
    authError("setup-error", "passwords do not match");
    return;
  }
  const r = await api("/api/setup", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ username: username, password: password }),
  });
  if (r.status !== 201) {
    authError("setup-error", (r.body.error && r.body.error.message) || r.status);
    return;
  }
  $("setup-form").reset();
  await refresh();
});

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  authError("login-error");
  const r = await api("/api/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      username: $("login-user").value.trim(),
      password: $("login-pass").value,
    }),
  });
  if (r.status !== 200) {
    authError("login-error", (r.body.error && r.body.error.message) || r.status);
    return;
  }
  $("login-form").reset();
  await refresh();
});

$("logout").addEventListener("click", async () => {
  // Origin/Host are checked server-side; a same-origin POST carries them.
  await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
  location.reload();
});

refresh();
