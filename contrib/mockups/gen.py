#!/usr/bin/env python3
"""arouter web UI — design pass. Emits every screen as SVG.
Palette is locked to logo.svg; layout is computed, never hand-placed.
Run: python3 contrib/mockups/gen.py   (writes contrib/mockups/*.svg)"""

BG, PANEL, BORDER = "#0D1117", "#161B22", "#30363D"
TXT, DIM = "#E6EDF3", "#8B949E"
GREEN, AMBER, BLUE, TEAL, RED = "#3FB950", "#E3B341", "#58A6FF", "#2DD4BF", "#F45067"
SANS = "-apple-system,'Segoe UI','Noto Sans',sans-serif"
MONO = "'JetBrains Mono','Fira Code',Consolas,'DejaVu Sans Mono',monospace"
GC = {"A+": GREEN, "A": AMBER, "B": BLUE}


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Doc:
    def __init__(self, w, h):
        self.w, self.h, self.e = w, h, []

    def rect(self, x, y, w, h, fill=PANEL, stroke=BORDER, rx=10, op=None):
        s = f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}"'
        if stroke: s += f' stroke="{stroke}"'
        if op: s += f' opacity="{op}"'
        self.e.append(s + "/>")

    def text(self, x, y, s, size=13, fill=TXT, anchor="start", font=SANS, weight=None):
        a = f' font-weight="{weight}"' if weight else ""
        self.e.append(f'<text x="{x}" y="{y}" text-anchor="{anchor}" font-size="{size}" '
                      f'fill="{fill}" font-family="{font}"{a}>{esc(s)}</text>')

    def chip(self, x, y, label, color, size=11):
        w = 12 + len(label) * (size * 0.60)
        self.rect(x, y, w, size + 12, fill=color, stroke=None, rx=4, op=0.16)
        self.text(x + w / 2, y + size + 2, label, size, fill=color, anchor="middle")
        return w

    def dot(self, x, y, color, r=6):
        self.e.append(f'<circle cx="{x}" cy="{y}" r="{r}" fill="{color}"/>')

    def bar(self, x, y, w, pct, color, h=8):
        self.rect(x, y, w, h, fill=BORDER, stroke=None, rx=4)
        self.rect(x, y, max(2, w * pct), h, fill=color, stroke=None, rx=4)

    def diamond(self, x, y, s=12):
        self.e.append(f'<polygon points="{x},{y-s} {x+s},{y} {x},{y+s} {x-s},{y}" fill="{TEAL}"/>')

    def switch(self, x, y, on):
        col = GREEN if on else RED
        self.rect(x, y, 74, 22, fill=col, stroke=None, rx=11, op=0.18)
        self.dot(x + (56 if on else 18), y + 11, col, 7)
        self.text(x + 37, y + 15, "on" if on else "off", 11, col, anchor="middle")

    def save(self, path):
        head = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.w} {self.h}" '
                f'width="{self.w}" height="{self.h}" font-family="{SANS}">')
        with open(path, "w") as f:
            f.write(head + "\n".join(self.e) + "\n</svg>\n")


def header(d, y, right=""):
    d.diamond(36, y + 4, 11)
    d.text(56, y + 8, "arouter", 17, TXT, weight="bold")
    if right:
        d.text(d.w - 24, y + 8, right, 12, DIM, anchor="end")


def panel(d, x, y, w, h, title=None, tcol=DIM):
    d.rect(x, y, w, h)
    if title:
        d.text(x + 16, y + 24, title, 13, tcol)


def footer(d, s):
    d.text(24, d.h - 22, s, 11, DIM)


# ─────────────────────────── 1 dashboard ──────────────────────────────
def s1():
    d = Doc(1160, 760); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "up · 477 models · 0.0.0.0:3929 · web on")
    y = 78
    for i, (g, n, pools) in enumerate([("A+", "12 models", "quota · ondemand"),
                                       ("A", "38 models", "quota · local · ondemand"),
                                       ("B", "427 models", "quota · local")]):
        x = 24 + i * 240
        d.rect(x, y, 224, 92)
        d.text(x + 16, y + 34, g, 24, GC[g], weight="bold")
        d.text(x + 16, y + 60, n, 12, DIM)
        d.text(x + 16, y + 80, pools, 11, DIM)
    y += 108
    panel(d, 24, y, 700, 148, "providers — quota windows")
    for i, (p, pct, reset, badge, col) in enumerate([("z.ai", .33, "↻ 1h17m", "⚡ drain", GREEN),
                                                     ("codex", .03, "↻ 2d", "", GREEN),
                                                     ("openrouter", .61, "↻ 9h", "pace ahead", AMBER)]):
        ry = y + 46 + i * 32
        d.text(40, ry + 4, p, 14, TXT)
        d.bar(170, ry, 240, pct, col)
        d.text(430, ry + 4, reset, 12, DIM)
        if badge: d.text(520, ry + 4, badge, 12, col)
    panel(d, 744, y, 392, 148, "live")
    for i, (k, v) in enumerate([("in flight", "2"), ("degraded 1h", "0"),
                                ("spend today", "$1.24"), ("uptime", "3d 4h")]):
        d.text(760, y + 48 + i * 24, k, 12, DIM)
        d.text(1120, y + 48 + i * 24, v, 14, TXT, anchor="end")
    y += 164
    panel(d, 24, y, 1112, 250, "models — grade · pool · enabled")
    for i, (m, g, pool, en, note) in enumerate([("glm-5.3", "A+", "quota", 1, "w 1.5"),
                                                ("glm-5.2", "A", "quota", 1, "w 1.0"),
                                                ("qwen3.8-flash", "B", "local", 1, "resident · warm"),
                                                ("or/claude-opus-4.6", "A+", "ondemand", 0, "$5/$25 · vip only")]):
        ry = y + 52 + i * 44
        d.dot(40, ry - 4, GREEN if en else RED)
        d.text(56, ry, m, 14, TXT, font=MONO)
        d.text(330, ry, g, 14, GC[g], weight="bold")
        d.text(380, ry, pool, 12, DIM)
        d.text(470, ry, "enabled" if en else "disabled", 12, GREEN if en else RED)
        d.text(600, ry, note, 11, DIM)
        d.text(1112, ry, "grade ▾   disable", 11, DIM, anchor="end")
    footer(d, "every change = POST /api/… · session cookie + same-Origin required")
    d.save("contrib/mockups/1-dashboard.svg")


# ─────────────────────────── 2 tokens ─────────────────────────────────
def s2():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "alice · 2 of 5 tokens")
    y = 78
    panel(d, 24, y, 540, 250, "your api tokens")
    for i, (name, tok, scopes, budget) in enumerate([("night-agent", "ark_••••••••••••••", "grades A+..A · pools quota · models all", "$2/day"),
                                                     ("ci-bot", "ark_••••••••••", "grade B · pools local · models ollama/*", "$0")]):
        ry = y + 52 + i * 92
        d.text(40, ry, name, 15, TXT, weight="bold")
        d.text(240, ry, tok, 13, DIM, font=MONO)
        d.text(40, ry + 24, scopes, 11, DIM)
        d.text(40, ry + 44, budget, 12, AMBER)
        d.text(540, ry, "revoke", 11, RED, anchor="end")
    panel(d, 584, y, 552, 250, "new token")
    for i, (k, v) in enumerate([("name", "night-agent"), ("grades", "A+ .. A"), ("pools", "quota · local"),
                                ("models", "all except ollama/*"), ("daily budget", "$2.00")]):
        d.text(600, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.rect(600, y + 176, 120, 30, fill=GREEN, stroke=None, rx=6, op=0.2)
    d.text(660, y + 196, "create", 13, GREEN, anchor="middle")
    y += 266
    panel(d, 24, y, 1112, 130, "what this token can and cannot do", TXT)
    for i, (mark, col, s) in enumerate([("✓", GREEN, "reaches glm-5.3, glm-5.2, qwen3.8-flash — whichever is healthiest at request time"),
                                        ("✗", RED, "can never reach the on-demand pool — spend stays $0 even if quota is exhausted"),
                                        ("✗", RED, "cannot see vip-only models, cannot raise its own budget")]):
        d.text(40, y + 52 + i * 26, mark + " " + s, 12, col)
    d.save("contrib/mockups/2-tokens.svg")


# ─────────────────────────── 3 admin ──────────────────────────────────
def s3():
    d = Doc(1160, 760); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "admin · session claim:xxxx")
    y = 78
    panel(d, 24, y, 540, 220, "users")
    for i, (u, role, spend, cap) in enumerate([("alice", "owner", "$1.24", "$5"),
                                               ("night-agent", "user", "$0.31", "$2"),
                                               ("ci-bot", "user", "$0.00", "$0.50")]):
        ry = y + 52 + i * 52
        d.text(40, ry, u, 14, TXT)
        d.text(240, ry, role, 12, TEAL)
        d.text(360, ry, spend, 12, DIM)
        d.text(470, ry, cap, 12, DIM)
        d.text(540, ry, "suspend", 11, RED, anchor="end")
    d.text(40, y + 196, "+ add user — name · project · daily cap · role", 12, DIM)
    panel(d, 584, y, 552, 220, "access rules — tokens may only narrow")
    for i, (rule, col) in enumerate([("A+ → quota pool only", AMBER),
                                     ("ollama/* → local tokens only", BLUE),
                                     ("glm-5.3 → disabled for ci-bot", RED)]):
        d.text(600, y + 52 + i * 28, rule, 13, col)
    d.text(600, y + 140, "auth mode: require api token  [ON]", 13, TXT)
    d.text(600, y + 164, "missing/unknown token → 401 · anonymous off", 11, DIM)
    y += 236
    panel(d, 24, y, 1112, 180, "providers — enable · quota source")
    for i, (p, on, src, ov) in enumerate([("z.ai", 1, "native zai", ""),
                                          ("strata", 1, "manual", "override ▾"),
                                          ("mystery-api", 0, "off", "enable ▾")]):
        ry = y + 52 + i * 40
        d.switch(40, ry - 12, bool(on))
        d.text(140, ry, p, 14, TXT)
        d.text(300, ry, src, 12, TEAL)
        if ov: d.text(700, ry, ov, 11, DIM)
    d.save("contrib/mockups/3-admin.svg")


# ─────────────────────────── 4 claim + login ──────────────────────────
def s4():
    d = Doc(1160, 620); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "first run")
    y = 76
    panel(d, 24, y, 540, 240, "claim this router")
    d.text(40, y + 56, "someone must own this router before anything else works.", 13, DIM)
    d.rect(40, y + 84, 500, 44, fill=BORDER, stroke=None, rx=6, op=0.5)
    d.text(60, y + 112, "http://127.0.0.1:3929/claim/8f2c71", 15, MONO and TEAL, font=MONO)
    d.text(40, y + 148, "single-use · works from localhost only · expires in 15 min", 11, DIM)
    d.text(40, y + 172, "the daemon prints this line under the banner, once.", 11, DIM)
    d.rect(40, y + 196, 160, 30, fill=TEAL, stroke=None, rx=6, op=0.2)
    d.text(120, y + 216, "open claim link", 12, TEAL, anchor="middle")
    panel(d, 584, y, 552, 240, "return visit — sign in")
    for i, (k, v) in enumerate([("session", "HttpOnly cookie · 12h sliding"),
                                ("writes", "cookie + same-Origin header required"),
                                ("no password in the URL", "nothing to leak into logs")]):
        d.text(600, y + 56 + i * 26, k, 12, DIM)
        d.text(1120, y + 56 + i * 26, v, 12, TXT, anchor="end")
    d.text(600, y + 150, "admin since 2026-10-11 · 2 users · auth require ON", 12, DIM)
    y += 256
    panel(d, 24, y, 1112, 120, "why this is safe on a LAN over plain HTTP", TXT)
    for i, s in enumerate(["transport is still trusted-network — keys protect identity, not the wire",
                           "writes are the thing that matters: they need a session and a same-Origin check",
                           "the claim link is the only unauthenticated door, and it is localhost-only and single-use"]):
        d.text(40, y + 52 + i * 26, "· " + s, 12, DIM)
    d.save("contrib/mockups/4-claim.svg")


# ─────────────────────────── 5 models browser + add ───────────────────
def s5():
    d = Doc(1160, 780); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "477 models · 3 grades · 6 providers")
    y = 76
    panel(d, 24, y, 1112, 44, "filter — grade ▾ · pool ▾ · provider ▾ · search \"glm\"")
    y += 56
    panel(d, 24, y, 640, 300, "models — sorted by grade, then weight")
    for i, (m, g, pool, en, note) in enumerate([("glm-5.3", "A+", "quota", 1, "z.ai · coding plan"),
                                                ("glm-5.2", "A", "quota", 1, "z.ai · coding plan"),
                                                ("glm-4.5-air", "B", "quota", 1, "z.ai · coding plan"),
                                                ("qwen3.8-flash", "B", "local", 1, "strata · llama.cpp"),
                                                ("or/llama-405b", "B", "ondemand", 0, "$0.15/$0.6 · off")]):
        ry = y + 56 + i * 46
        d.dot(40, ry - 4, GREEN if en else RED)
        d.text(56, ry, m, 14, TXT, font=MONO)
        d.text(300, ry, g, 14, GC[g], weight="bold")
        d.text(350, ry, pool, 12, DIM)
        d.text(470, ry, note, 11, DIM)
        d.text(648, ry, "edit grade", 11, TEAL, anchor="end")
    panel(d, 684, y, 452, 300, "add a model")
    for i, (k, v) in enumerate([("id", "z.ai/glm-5.4"), ("grade", "A+"), ("pool", "quota"),
                                ("context", "200k"), ("pricing", "in $0.6 · out $1.8 / Mtok"),
                                ("quota source", "native zai")]):
        d.text(700, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.rect(700, y + 226, 150, 30, fill=GREEN, stroke=None, rx=6, op=0.2)
    d.text(775, y + 246, "add model", 12, GREEN, anchor="middle")
    d.text(700, y + 272, "goes to the managed overlay — your config.toml untouched", 11, DIM)
    y += 316
    panel(d, 24, y, 1112, 140, "grade adjust — preview before it applies", TXT)
    d.text(40, y + 40, "glm-5.3   A+ → A", 15, TXT)
    d.text(40, y + 66, "what changes: 38 → 50 models at A · A+ now served by glm-5.2, or/opus-4.6", 12, DIM)
    d.text(40, y + 90, "what does not change: pools, budgets, which providers exist", 12, DIM)
    d.rect(40, y + 106, 200, 28, fill=TEAL, stroke=None, rx=6, op=0.18)
    d.text(140, y + 124, "apply · undo", 12, TEAL, anchor="middle")
    footer(d, "discovered models keep weight 0.5 and never outrank a graded model")
    d.save("contrib/mockups/5-models.svg")


# ─────────────────────────── 6 providers + quota ──────────────────────
def s6():
    d = Doc(1160, 760); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "6 providers · quota poll every 15 min")
    y = 76
    panel(d, 24, y, 1112, 260, "providers — source of truth for quota")
    for i, (p, on, src, note, col) in enumerate([("z.ai", 1, "native", "2 windows · 5h + weekly", GREEN),
                                                 ("openrouter", 1, "native (documented)", "key limits + credits", GREEN),
                                                 ("codex", 1, "headers", "x-ratelimit-* harvested from responses", GREEN),
                                                 ("strata", 1, "manual", "local engine — no quota API", BLUE),
                                                 ("mystery-api", 0, "off", "no key configured", RED)]):
        ry = y + 56 + i * 44
        d.switch(40, ry - 12, bool(on))
        d.text(140, ry, p, 14, TXT)
        d.text(300, ry, src, 12, TEAL)
        d.text(560, ry, note, 11, DIM)
        d.text(1120, ry, "windows ▾", 11, DIM, anchor="end")
    y += 276
    panel(d, 24, y, 540, 220, "quota override — when no API exists")
    for i, (k, v) in enumerate([("provider", "strata"), ("window", "weekly"),
                                ("used", "0%"), ("resets", "never"), ("source", "manual")]):
        d.text(40, y + 52 + i * 26, k, 12, DIM)
        d.text(540, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.text(40, y + 186, "merged as a synthetic sample — routing math is identical", 11, DIM)
    panel(d, 584, y, 552, 220, "etiquette — what we actually send")
    for i, s in enumerate(["own key, own host — a credential never leaves its provider's base URL",
                           "one request per provider per poll, backoff on a 429 of the quota call itself",
                           "undocumented endpoints are opt-in per provider, default off",
                           "failure keeps the last snapshot — quota loss never breaks routing"]):
        d.text(600, y + 52 + i * 26, "· " + s, 12, DIM)
    d.save("contrib/mockups/6-providers.svg")


# ─────────────────────────── 7 access rules ───────────────────────────
def s7():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "access rules · admin sets the ceiling, tokens only narrow")
    y = 76
    panel(d, 24, y, 640, 240, "global rules")
    for i, (rule, col) in enumerate([("A+ → quota pool only · ondemand: vip tokens", AMBER),
                                     ("ollama/* → local tokens only", BLUE),
                                     ("glm-5.3 → disabled for ci-bot", RED),
                                     ("vip tier → may cross into ondemand", TEAL)]):
        d.text(40, y + 52 + i * 30, rule, 13, col)
    d.text(40, y + 190, "a rule can only remove candidates — never add a pool a token lacks", 11, DIM)
    panel(d, 684, y, 452, 240, "narrow with a token")
    for i, (k, v) in enumerate([("token", "night-agent"), ("grades", "A+ .. A"), ("pools", "quota"),
                                ("models", "all except ollama/*")]):
        d.text(700, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.text(700, y + 170, "effective reach = rules ∩ token scopes", 12, DIM)
    y += 256
    panel(d, 24, y, 1112, 180, "who can reach what — computed, never hand-set", TXT)
    cols = ["glm-5.3", "glm-5.2", "qwen3.8", "or/opus-4.6"]
    rows = [("alice (owner)", [1, 1, 1, 1]), ("night-agent", [1, 1, 1, 0]), ("ci-bot", [0, 1, 1, 0])]
    for j, c in enumerate(cols):
        d.text(300 + j * 180, y + 44, c, 12, DIM, anchor="middle")
    for i, (u, cells) in enumerate(rows):
        ry = y + 76 + i * 34
        d.text(40, ry, u, 13, TXT)
        for j, v in enumerate(cells):
            d.dot(300 + j * 180, ry - 4, GREEN if v else RED, 7)
    footer(d, "the matrix is derived at request time — it is a view, not a second source of truth")
    d.save("contrib/mockups/7-access.svg")


# ─────────────────────────── 8 users ──────────────────────────────────
def s8():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "users · spend is per caller, caps stack")
    y = 76
    panel(d, 24, y, 640, 260, "callers")
    for i, (u, role, spend, cap, tok) in enumerate([("alice", "owner", "$1.24", "$5", "3"),
                                                    ("night-agent", "user", "$0.31", "$2", "1"),
                                                    ("ci-bot", "user", "$0.00", "$0.50", "1"),
                                                    ("guest-4", "suspended", "—", "—", "0")]):
        ry = y + 56 + i * 52
        d.text(40, ry, u, 14, TXT)
        d.text(280, ry, role, 12, TEAL if role != "suspended" else RED)
        d.text(420, ry, spend, 12, DIM)
        d.text(540, ry, cap, 12, DIM)
        d.text(648, ry, tok + " token", 11, DIM, anchor="end")
    d.text(40, y + 232, "+ add user — name · project · email? · daily cap · role", 12, DIM)
    panel(d, 684, y, 452, 260, "add user")
    for i, (k, v) in enumerate([("name", "night-shift"), ("project", "home lab"),
                                ("daily cap", "$2.00"), ("role", "user"),
                                ("auth", "token (generated on save)")]):
        d.text(700, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.rect(700, y + 190, 170, 30, fill=GREEN, stroke=None, rx=6, op=0.2)
    d.text(785, y + 210, "create user", 12, GREEN, anchor="middle")
    y += 276
    panel(d, 24, y, 1112, 160, "spend — 24h per caller, caps stack min(pool, profile, caller)")
    for i, (u, v, cap) in enumerate([("alice", .25, "$5"), ("night-agent", .16, "$2"),
                                     ("ci-bot", .02, "$0.50")]):
        ry = y + 56 + i * 30
        d.text(40, ry, u, 12, DIM)
        d.bar(240, ry - 6, 600, v, AMBER)
        d.text(880, ry, f"$ {v*20:.2f} of {cap}", 11, DIM)
    footer(d, "a caller that exceeds its cap gets 402 — the response names the cap, not the provider")
    d.save("contrib/mockups/8-users.svg")


# ─────────────────────────── 9 settings ───────────────────────────────
def s9():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "settings · admin only")
    y = 76
    panel(d, 24, y, 540, 240, "access control")
    d.switch(40, y + 52, True); d.text(140, y + 64, "require api token", 12, DIM)
    d.switch(40, y + 92, False); d.text(140, y + 104, "anonymous allowed", 12, DIM)
    d.switch(40, y + 132, False); d.text(140, y + 144, "cross_pool into on-demand", 12, DIM)
    d.text(40, y + 180, "off = anyone on the LAN can route · on = 401 without a token", 11, DIM)
    d.text(40, y + 204, "changing this logs the admin, the session and the time", 11, DIM)
    panel(d, 584, y, 552, 240, "quota engine")
    for i, (k, v) in enumerate([("poll", "every 15 min"), ("sources", "native · headers · manual"),
                                ("undocumented", "opt-in per provider"), ("on failure", "keep last snapshot")]):
        d.text(600, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.text(600, y + 170, "no external quota tool — the router asks its own upstreams", 12, DIM)
    y += 256
    panel(d, 24, y, 1112, 180, "budgets — the paid pool stays guarded")
    for i, (k, v) in enumerate([("pool on-demand", "$5/day · $25/month"),
                                ("profile arouter-night-ondemand", "$1/day · $5/month"),
                                ("caller alice", "$5/day · $20/month"),
                                ("pace window", "1h rolling")]):
        d.text(40, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    d.text(40, y + 156, "effective cap = min(pool, profile, caller) — a token can only lower it", 11, DIM)
    footer(d, "settings write to the managed overlay · your config.toml is never rewritten")
    d.save("contrib/mockups/9-settings.svg")


# ─────────────────────────── 10 decision trace ────────────────────────
def s10():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "why this model was chosen")
    y = 76
    panel(d, 24, y, 1112, 60, "POST /v1/chat/completions · model A+ · caller night-agent · token ark_•••")
    y += 72
    panel(d, 24, y, 1112, 300, "candidates — grade is the primary sort key, everything else reorders within it")
    rows = [("glm-5.3", "quota", "avail 1.0 × health 0.9 × weight 1.5 × drain 1.4", "= 1.89", "chosen", GREEN),
            ("glm-5.2", "quota", "avail 1.0 × health 0.9 × weight 1.0", "= 0.90", "", DIM),
            ("or/claude-opus-4.6", "ondemand", "pool outside token scope", "excluded", RED),
            ("qwen3.8-flash", "local", "grade B below request A+ — not eligible", "excluded", RED),
            ("glm-4.6", "quota", "window exhausted · resets in 1h17m", "excluded", RED)]
    for i, (m, pool, why, score, tag, col) in enumerate(rows):
        ry = y + 56 + i * 46
        d.text(40, ry, m, 14, TXT, font=MONO)
        d.text(300, ry, pool, 11, DIM)
        d.text(420, ry, why, 12, DIM)
        d.text(880, ry, score, 12, col)
        if tag: d.text(1120, ry, "✓ " + tag if tag == "chosen" else tag, 12, GREEN if tag == "chosen" else RED, anchor="end")
    y += 316
    panel(d, 24, y, 1112, 160, "the drain rule — use it or lose it", TXT)
    d.text(40, y + 44, "z.ai has 33% left and its 5h window resets in 1h17m — that quota is a wasting asset.", 13, DIM)
    d.text(40, y + 70, "so penalties lift and ties resolve toward the soonest reset: spend it now, not after it rolls.", 13, DIM)
    d.text(40, y + 96, "exhaustion still hard-stops the candidate; drain only reorders inside a grade — it never crosses pools.", 12, DIM)
    footer(d, "same numbers the monitor TUI and the Grafana gauges show — one source of truth")
    d.save("contrib/mockups/10-trace.svg")


# ─────────────────────────── 11 refusal states ────────────────────────
def s11():
    d = Doc(1160, 620); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "what a caller sees when we say no")
    y = 76
    for i, (code, title, body, fix, col) in enumerate([
        ("401", "no valid token", "arouter requires an api token on this network", "create a token at /tokens"),
        ("403", "outside token scope", "this token may not reach the on-demand pool", "use a token that may, or lower the grade"),
        ("402", "budget spent", "caller night-agent hit its $2/day cap", "raise the cap, or wait for the window"),
        ("403", "model disabled", "glm-5.3 is disabled by the owner", "ask them, or pick another grade")]):
        x = 24 + i * 280
        d.rect(x, y, 264, 200)
        d.text(x + 16, y + 34, code, 20, col, weight="bold")
        d.text(x + 16, y + 62, title, 13, TXT)
        d.text(x + 16, y + 92, body, 11, DIM)
        d.text(x + 16, y + 122, fix, 11, TEAL)
        d.text(x + 16, y + 156, "Retry-After: 4620", 11, DIM, font=MONO)
    y += 216
    panel(d, 24, y, 1112, 140, "the refusal never names a provider")
    for i, s in enumerate(["the wire response says pool and grade — the upstream identity stays in the header and the monitor",
                           "pi's UI keeps showing the grade: the caller learns it was refused, not who would have served it"]):
        d.text(40, y + 52 + i * 26, "· " + s, 12, DIM)
    d.save("contrib/mockups/11-refusals.svg")


# ─────────────────────────── 12 mobile ────────────────────────────────
def s12():
    d = Doc(420, 900); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 20, "up · 477")
    y = 70
    for i, (g, n) in enumerate([("A+", "12 models"), ("A", "38 models"), ("B", "427 models")]):
        d.rect(24, y + i * 76, 372, 64)
        d.text(40, y + i * 76 + 26, g, 22, GC[g], weight="bold")
        d.text(40, y + i * 76 + 50, n, 12, DIM)
    y += 244
    panel(d, 24, y, 372, 170, "quota")
    for i, (p, pct, reset, badge, col) in enumerate([("z.ai", .33, "↻ 1h17m", "⚡ drain", GREEN),
                                                     ("codex", .03, "↻ 2d", "", GREEN)]):
        ry = y + 48 + i * 44
        d.text(40, ry, p, 13, TXT)
        d.bar(150, ry + 4, 180, pct, col)
        d.text(340, ry + 4, reset, 11, DIM)
        if badge: d.text(340, ry + 22, badge, 11, col)
    y += 186
    panel(d, 24, y, 372, 150, "spend today")
    d.text(40, y + 52, "$1.24", 22, TXT, weight="bold")
    d.text(40, y + 84, "alice $1.24 · night-agent $0.31", 12, DIM)
    d.text(40, y + 112, "caps $5 / $2 — both open", 11, DIM)
    y += 166
    panel(d, 24, y, 372, 120, "in flight")
    d.text(40, y + 52, "2 requests · 0 degraded (1h)", 13, TXT)
    d.text(40, y + 84, "last: A+ → glm-5.3 · drain", 12, DIM)
    footer(d, "one column, same numbers as the TUI · tap a card to open its screen")
    d.save("contrib/mockups/12-mobile.svg")


# ─────────────────── 13 install (terminal) ───────────────────
def s13():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "install · one command, no sudo")
    y = 76
    d.rect(24, y, 700, 300, fill="#0B0F14")
    d.text(40, y + 30, "$ curl -fsSL https://arouter.sh/install.sh | sh", 14, TEAL, font=MONO)
    lines = [("build", "cargo build --release --locked", "ok · 2m11s", GREEN),
             ("place", "~/.local/bin/arouter", "4.8 MB · mode 755", DIM),
             ("init", "~/.config/arouter/config.toml", "written · 0600", GREEN),
             ("doctor", "12 checks", "0 errors · 2 warnings", AMBER),
             ("serve", "daemon up · 0.0.0.0:3929", "systemd unit optional", DIM)]
    for i, (a, b, c, col) in enumerate(lines):
        ry = y + 62 + i * 34
        d.text(40, ry, "  " + a.ljust(8), 13, TXT, font=MONO)
        d.text(150, ry, b, 12, DIM, font=MONO)
        d.text(700, ry, c, 12, col, anchor="end")
    d.text(40, y + 246, "  web     claim http://127.0.0.1:3929/claim/8f2c71", 13, TEAL, font=MONO)
    d.text(40, y + 272, "          \u2191 open from this machine — the only unclaimed door", 11, DIM, font=MONO)
    panel(d, 744, y, 392, 300, "what install.sh deliberately does not do")
    for i, s in enumerate(["no sudo, no /usr/bin, no PATH edit", "PREFIX= override for the careful",
                           "--skip-init for a config you already own", "init is idempotent — never overwrites",
                           "nothing starts until you say serve"]):
        d.text(760, y + 52 + i * 26, "· " + s, 12, DIM)
    y += 316
    panel(d, 24, y, 1112, 160, "arouter doctor — the gate before anything routes", TXT)
    for i, (k, v, col) in enumerate([("config parse", "ok · 477 models catalogued", GREEN),
                                     ("keys", "zai ok · openrouter ok · codex missing", AMBER),
                                     ("quota sources", "zai native · strata manual · codex headers", GREEN),
                                     ("bind", "0.0.0.0 — LAN exposed, auth require off", AMBER)]):
        d.text(40, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 12, col, anchor="end")
    footer(d, "warnings never block: the router runs, and tells you what it could not see")
    d.save("contrib/mockups/13-install.svg")


# ─────────────────── 14 first access (browser) ───────────────────
def s14():
    d = Doc(1160, 700); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "first visit · no admin exists yet")
    y = 76
    d.rect(24, y, 1112, 44, fill=BORDER, stroke=None, rx=6, op=0.6)
    d.text(40, y + 28, "http://192.168.50.192:3929/", 14, TEAL, font=MONO)
    y += 60
    panel(d, 24, y, 640, 260, "this router is not yours yet")
    d.text(40, y + 56, "Nobody has claimed it. Routing already works — management does not.", 13, DIM)
    d.text(40, y + 84, "open mode: any caller on the network can ask for a grade", GREEN)
    d.text(40, y + 110, "no admin, no users, no tokens — nothing to manage yet", DIM)
    d.text(40, y + 140, "to manage it, open the claim line the daemon printed:", TXT)
    d.rect(40, y + 160, 480, 34, fill=BORDER, stroke=None, rx=6, op=0.5)
    d.text(56, y + 184, "http://127.0.0.1:3929/claim/8f2c71", 14, TEAL, font=MONO)
    d.text(40, y + 216, "that link answers only to localhost · single-use · 15 min", 11, DIM)
    panel(d, 684, y, 452, 260, "why the browser can be told \"no\"")
    for i, s in enumerate(["opened from a phone on the LAN → link refuses (not localhost)",
                           "link already used → refuses, and says which step failed",
                           "daemon restarted → the old code is gone, a new one prints",
                           "no claim wanted? nothing breaks — the web layer is optional"]):
        d.text(700, y + 52 + i * 26, "· " + s, 12, DIM)
    y += 276
    panel(d, 24, y, 1112, 140, "the claim is the only unauthenticated door in the whole app", TXT)
    for i, s in enumerate(["it grants nothing by itself — the code printed in the terminal must be pasted too",
                           "everything after it needs a session cookie + same-Origin check"]):
        d.text(40, y + 52 + i * 26, "· " + s, 12, DIM)
    d.save("contrib/mockups/14-first-access.svg")


# ─────────────────── 15 claim + owner setup ───────────────────
def s15():
    d = Doc(1160, 760); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "claim · step 2 of 2")
    y = 76
    panel(d, 24, y, 540, 240, "prove this machine owns the router")
    d.text(40, y + 52, "1 · link opened from localhost", 13, GREEN)
    d.text(40, y + 80, "2 · paste the code the daemon printed in your terminal", TXT)
    d.rect(40, y + 100, 240, 36, fill=BORDER, stroke=None, rx=6, op=0.5)
    d.text(56, y + 126, "8f2c71", 15, TEAL, font=MONO)
    d.text(40, y + 156, "the code is the second factor — the link alone grants nothing", 11, DIM)
    d.text(40, y + 184, "3 wrong tries left before the claim expires", AMBER)
    panel(d, 584, y, 552, 240, "name the owner")
    for i, (k, v) in enumerate([("name", "alice"), ("project", "home lab"),
                                ("role", "owner (only one per router)")]):
        d.text(600, y + 52 + i * 26, k, 12, DIM)
        d.text(1120, y + 52 + i * 26, v, 13, TXT, anchor="end")
    y += 256
    panel(d, 24, y, 1112, 200, "the password — set here, never in the URL", TXT)
    for i, s in enumerate(["min 12 characters · no rules theatre, no expiry games",
                           "typed into a form over the same origin · never a query string",
                           "stored as argon2id in the managed overlay · file mode 0600",
                           "nothing in the app, the logs or the config is ever plaintext"]):
        d.text(40, y + 52 + i * 26, "· " + s, 12, DIM)
    d.rect(40, y + 128, 300, 34, fill=BORDER, stroke=None, rx=6, op=0.5)
    d.text(56, y + 152, "••••••••••••", 13, DIM, font=MONO)
    d.text(40, y + 178, "change it later at /settings · sessions stay valid until it rotates", 11, DIM)
    y += 216
    panel(d, 24, y, 1112, 120, "what the claim grants the moment it succeeds")
    for i, s in enumerate(["you are the owner — the only role that can add users, tokens and rules",
                           "a session starts immediately: no second login round-trip",
                           "auth mode stays OFF by default: routing keeps working, management is now yours"]):
        d.text(40, y + 52 + i * 26, "· " + s, 12, DIM)
    d.save("contrib/mockups/15-claim-setup.svg")


# ─────────────────── 16 owner summary (post-claim) ───────────────────
def s16():
    d = Doc(1160, 620); d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=16)
    header(d, 22, "claimed · alice is the owner")
    y = 76
    panel(d, 24, y, 540, 220, "what is true now")
    for i, (k, v) in enumerate([("owner", "alice · since 2026-10-11"),
                                ("session", "signed in · sliding 12h"),
                                ("password", "argon2id · ~/.config/arouter/web.toml · 0600"),
                                ("auth mode", "open — routing unchanged")]):
        d.text(40, y + 52 + i * 26, k, 12, DIM)
        d.text(540, y + 52 + i * 26, v, 12, TXT, anchor="end")
    panel(d, 584, y, 552, 220, "next, in the order that matters")
    for i, (s, col) in enumerate([("restrict access — turn auth on when you want tokens", AMBER),
                                  ("issue tokens per agent, each with its own reach", TEAL),
                                  ("add users for people who need a cap", TEAL),
                                  ("disable the models you never want served", TEAL)]):
        d.text(600, y + 52 + i * 28, "→ " + s, 12, col)
    y += 236
    panel(d, 24, y, 1112, 140, "the overlay is the only file this screen writes", TXT)
    d.text(40, y + 44, "~/.config/arouter/config.toml   yours · hand-written · never rewritten", GREEN)
    d.text(40, y + 70, "~/.config/arouter/web.toml      managed · 0600 · owner, users, tokens, rules, overrides", TEAL)
    d.text(40, y + 96, "precedence: manual config > overlay > discovered — a web change cannot shadow your file", DIM)
    footer(d, "undo is per change: every write reports the file and the key it touched")
    d.save("contrib/mockups/16-owner.svg")


for s in (s1, s2, s3, s4, s5, s6, s7, s8, s9, s10, s11, s12, s13, s14, s15, s16):
    s()
print("16 screens written")
