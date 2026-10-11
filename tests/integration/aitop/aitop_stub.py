#!/usr/bin/env python3
"""aitop stub: serves switchable quota fixtures for the arouter quota
watcher (`curl -s http://aitop/quotas.json`).

    curl -X POST localhost:80/__mode -d '{"mode":"codex_dead"}'
    curl localhost:80/quotas.json

Fixtures use far-future reset timestamps (2100) so "exhausted" logic is
deterministic; `scarce` stays servable (90%) which the tests assert.
`zai_draining` computes a near-future reset at request time so the drain
rule (use-it-or-lose-it within DRAIN_SECS) fires deterministically.
"""
import json
import threading
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

FAR = "2100-01-01T00:00:00Z"


def near(hours):
    t = datetime.now(timezone.utc).replace(microsecond=0) + timedelta(hours=hours)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")

def row(label, pct, cap="", pace="", reset=FAR):
    r = {"label": label, "pct": pct, "cap": cap, "pace": pace}
    if reset is not None:
        r["reset_at"] = reset
    return r

FIXTURES = {
    # codex clean + lightly used; zai lightly used but pace-penalized →
    # at grade A the composite favors codex (0.75 < 1.0), and even without
    # the penalty, headroom (97% x far reset vs 67%) prefers codex. This is
    # the negative baseline the drain fixture flips.
    "healthy": {
        "fetched_at": FAR,
        "panels": [
            {"name": "codex", "rows": [row("5h window", 3, "150"), row("7d window", 16, "100")]},
            {"name": "zai", "rows": [row("5h window", 33, "120"), row("daily", 40, "500", "pace ahead 20%")]},
        ],
    },
    # codex fully consumed → arouter must skip stuba/* entirely
    "codex_dead": {
        "fetched_at": FAR,
        "panels": [
            {"name": "codex", "rows": [row("5h window", 100, "150", "pace behind 30%"), row("7d window", 16, "100")]},
            {"name": "zai", "rows": [row("5h window", 33, "120"), row("daily", 40, "500")]},
        ],
    },
    # 90% consumed: scarce penalty but still routable
    "codex_scarce": {
        "fetched_at": FAR,
        "panels": [
            {"name": "codex", "rows": [row("5h window", 90, "150"), row("7d window", 16, "100")]},
            {"name": "zai", "rows": [row("5h window", 33, "120"), row("daily", 40, "500")]},
        ],
    },
    # zai 5h window 33% but resetting in ~2h. The drain rule lifts zai's
    # pace penalty (drain makes pace meaningless for a dying window), the
    # composite ties 1.0 vs 1.0, and the drain axis — ranked above headroom
    # — steers same-grade A requests to zai, beating codex's headroom.
    "zai_draining": {
        "fetched_at": FAR,
        "panels": [
            {"name": "codex", "rows": [row("5h window", 3, "150"), row("7d window", 16, "100")]},
            {"name": "zai", "rows": [row("5h window", 33, "120", reset=near(2)), row("daily", 40, "500", "pace ahead 20%")]},
        ],
    },
}


def fixtures():
    # refresh the near-future reset lazily so long-lived rigs stay in the
    # drain horizon
    out = json.loads(json.dumps(FIXTURES))
    out["zai_draining"]["panels"][1]["rows"][0]["reset_at"] = near(2)
    return out

lock = threading.Lock()
mode = {"m": "healthy"}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _body(self):
        n = int(self.headers.get("content-length") or 0)
        try:
            return json.loads(self.rfile.read(n) if n else b"{}")
        except Exception:
            return {}

    def do_GET(self):
        if self.path == "/quotas.json":
            with lock:
                self._json(200, fixtures()[mode["m"]])
        elif self.path == "/__mode":
            with lock:
                self._json(200, {"mode": mode["m"]})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        if self.path == "/__mode":
            m = self._body().get("mode")
            if m in FIXTURES:
                with lock:
                    mode["m"] = m
                self._json(200, {"mode": m})
            else:
                self._json(400, {"error": f"unknown mode {m!r}"})
        else:
            self._json(404, {"error": "not found"})


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", 80), H).serve_forever()
