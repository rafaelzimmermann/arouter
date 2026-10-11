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
}

$("logout").addEventListener("click", async () => {
  // Origin/Host are checked server-side; a same-origin POST carries them.
  await fetch("/api/logout", { method: "POST", credentials: "same-origin" });
  location.reload();
});

refresh();
