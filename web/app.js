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

async function refresh() {
  $("host").textContent = location.host;
  const s = await api("/api/session");
  if (s.status !== 200) {
    $("panel").hidden = true;
    $("login").hidden = false;
    $("who").textContent = "";
    return;
  }
  $("who").textContent = s.body.role + " · expires in " +
    Math.max(0, Math.round((s.body.expires_unix - Date.now() / 1000) / 60)) + "m";
  const o = await api("/api/overview");
  if (o.status === 200) renderOverview(o.body);
  renderTokens();
}

$("logout").addEventListener("click", async () => {
  // Origin/Host are checked server-side; a same-origin POST carries them.
  await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
  location.reload();
});

refresh();
