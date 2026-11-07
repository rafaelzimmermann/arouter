#!/usr/bin/env python3
"""arouter web console — rework mockups.

Every screen is generated as SVG so the review copy stays diffable and
the layout is computed rather than hand-placed.  Open index.html for the
gallery, or any *.svg directly in a browser.

    python3 contrib/mockups/web-v2/gen.py
"""

from pathlib import Path

HERE = Path(__file__).resolve().parent

# ── palette (same family as the shipped shell / logo) ─────────────────
BG = "#0D1117"
PANEL = "#161B22"
PANEL2 = "#1B222C"
BORDER = "#30363D"
TXT = "#E6EDF3"
DIM = "#8B949E"
FAINT = "#6E7681"
GREEN = "#3FB950"
AMBER = "#E3B341"
BLUE = "#58A6FF"
TEAL = "#2DD4BF"
RED = "#F45067"
PURPLE = "#BC8CFF"
PRIMARY = "#1F6FEB"

SANS = "-apple-system,'Segoe UI','Noto Sans',sans-serif"
MONO = "'JetBrains Mono','Fira Code',Consolas,'DejaVu Sans Mono',monospace"

W, H = 1280, 820


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Doc:
    def __init__(self, w=W, h=H):
        self.w, self.h, self.e = w, h, []

    def raw(self, s):
        self.e.append(s)

    def rect(self, x, y, w, h, fill=PANEL, stroke=BORDER, rx=10, op=None):
        s = (f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" '
             f'fill="{fill}"')
        if stroke:
            s += f' stroke="{stroke}"'
        if op is not None:
            s += f' opacity="{op}"'
        self.e.append(s + "/>")

    def line(self, x1, y1, x2, y2, color=BORDER, width=1, dash=None, op=None):
        s = (f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" '
             f'stroke="{color}" stroke-width="{width}"')
        if dash:
            s += f' stroke-dasharray="{dash}"'
        if op is not None:
            s += f' opacity="{op}"'
        self.e.append(s + "/>")

    def path(self, d, fill="none", stroke=None, width=1.5, dash=None, op=None):
        s = f'<path d="{d}" fill="{fill}"'
        if stroke:
            s += f' stroke="{stroke}" stroke-width="{width}" stroke-linecap="round" stroke-linejoin="round"'
        if dash:
            s += f' stroke-dasharray="{dash}"'
        if op is not None:
            s += f' opacity="{op}"'
        self.e.append(s + "/>")

    def text(self, x, y, s, size=13, fill=TXT, anchor="start", font=SANS,
             weight=None, op=None, spacing=None):
        attrs = [f'x="{x}"', f'y="{y}"', f'text-anchor="{anchor}"',
                 f'font-size="{size}"', f'fill="{fill}"', f'font-family="{font}"']
        if weight:
            attrs.append(f'font-weight="{weight}"')
        if op is not None:
            attrs.append(f'opacity="{op}"')
        if spacing:
            attrs.append(f'letter-spacing="{spacing}"')
        self.e.append(f'<text {" ".join(attrs)}>{esc(s)}</text>')

    def circle(self, cx, cy, r, fill=None, stroke=None, width=1, op=None):
        s = f'<circle cx="{cx}" cy="{cy}" r="{r}"'
        s += f' fill="{fill or "none"}"'
        if stroke:
            s += f' stroke="{stroke}" stroke-width="{width}"'
        if op is not None:
            s += f' opacity="{op}"'
        self.e.append(s + "/>")

    def chip(self, x, y, label, color, size=11, w=None, fill_op=0.14):
        width = w if w is not None else 14 + len(label) * size * 0.59
        self.rect(x, y, width, size + 12, fill=color, stroke=None, rx=4, op=fill_op)
        self.text(x + width / 2, y + size + 2, label, size, color, anchor="middle")
        return width

    def dot(self, x, y, color, r=4):
        self.circle(x, y, r, fill=color)

    def bar(self, x, y, w, pct, color, h=6):
        self.rect(x, y, w, h, fill=BORDER, stroke=None, rx=h / 2)
        if pct > 0:
            self.rect(x, y, max(3, w * pct), h, fill=color, stroke=None, rx=h / 2)

    def button(self, x, y, w, label, kind="secondary", h=34, size=12):
        if kind == "primary":
            fill, stroke, txt = PRIMARY, None, "#FFFFFF"
        elif kind == "good":
            fill, stroke, txt = GREEN, None, "#08120A"
        elif kind == "danger":
            fill, stroke, txt = RED, None, "#1A070A"
        elif kind == "ghost":
            fill, stroke, txt = BG, BORDER, DIM
        else:
            fill, stroke, txt = PANEL2, BORDER, TXT
        self.rect(x, y, w, h, fill=fill, stroke=stroke, rx=6, op=None if kind in ("primary", "good", "danger") else None)
        self.text(x + w / 2, y + h / 2 + 4, label, size, txt, anchor="middle", weight="bold" if kind in ("primary", "good", "danger") else None)

    def checkbox(self, x, y, on=False, color=BLUE):
        self.rect(x, y, 15, 15, fill=BLUE if on else BG, stroke=BORDER, rx=3)
        if on:
            self.path(f"M {x+3.5} {y+7.5} L {x+6.5} {y+10.5} L {x+12} {y+4.5}", stroke="#fff", width=1.8)

    def switch(self, x, y, on=True, w=42):
        col = GREEN if on else FAINT
        self.rect(x, y, w, 22, fill=col, stroke=None, rx=11, op=0.25)
        self.circle(x + (w - 9 if on else 9), y + 11, 7, fill=col)
        self.text(x + (w - 30 if on else 30), y + 15, "on" if on else "off", 10,
                  col, anchor="middle")

    def spark(self, x, y, w, h, vals, color=BLUE):
        if not vals:
            return
        mn, mx = min(vals), max(vals)
        rng = (mx - mn) or 1
        pts = []
        for i, v in enumerate(vals):
            px = x + (w * i / max(1, len(vals) - 1))
            py = y + h - ((v - mn) / rng) * h
            pts.append((px, py))
        poly = " ".join(f"{px:.1f},{py:.1f}" for px, py in pts)
        pts_area = [(x, y + h)] + pts + [(x + w, y + h)]
        area = " L ".join(f"{px:.1f},{py:.1f}" for px, py in pts_area)
        self.path(f"M {area} Z", fill=color, op=0.12, stroke=None)
        self.e.append(f'<polyline points="{poly}" fill="none" stroke="{color}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/>')

    def donut(self, cx, cy, r, parts, width=14):
        import math
        circ = 2 * math.pi * r
        self.circle(cx, cy, r, fill="none", stroke=BORDER, width=width)
        off = 0
        for pct, color in parts:
            seg = circ * pct
            self.raw(f'<circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="{color}" '
                     f'stroke-width="{width}" stroke-dasharray="{seg:.2f} {circ:.2f}" '
                     f'stroke-dashoffset="{-off:.2f}" transform="rotate(-90 {cx} {cy})"/>')
            off += seg

    def save(self, name):
        head = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.w} {self.h}" '
                f'width="{self.w}" height="{self.h}" font-family="{SANS}">')
        (HERE / name).write_text(head + "\n".join(self.e) + "\n</svg>\n")


# ── shared chrome ─────────────────────────────────────────────────────
NAV = [
    ("OPERATE", ["Overview", "Models", "Providers & quota", "Routing"]),
    ("ACCESS", ["API keys", "Users"]),
    ("SYSTEM", ["Settings"]),
]


def topbar(d):
    d.rect(0, 0, W, 64, fill=PANEL, stroke=None, rx=0)
    d.line(0, 64, W, 64)
    # logo
    d.raw('<polygon points="34,24 44,32 34,40 24,32" fill="#2DD4BF"/>')
    d.text(54, 37, "arouter", 18, TXT, weight="bold")
    d.chip(143, 21, "console", BLUE, 10)
    d.dot(233, 32, GREEN, 4)
    d.text(243, 36, "thepotato · production", 11, DIM)
    # search
    d.rect(716, 16, 286, 32, fill=BG, stroke=BORDER, rx=7)
    d.circle(735, 32, 5, stroke=DIM, width=1.5)
    d.line(739, 36, 744, 41, DIM, 1.5)
    d.text(754, 37, "Search models, providers, keys…", 11, FAINT)
    d.chip(1152, 20, "⌘ K", FAINT, 10, w=34)
    # health + avatar
    d.chip(1018, 19, "●  Serving normally", GREEN, 10)
    d.circle(1240, 32, 15, fill=TEAL, op=0.18)
    d.text(1240, 37, "A", 12, TEAL, anchor="middle", weight="bold")


def sidebar(d, active):
    d.rect(0, 64, 232, H - 64, fill=PANEL, stroke=None, rx=0)
    d.line(232, 64, 232, H)
    y = 96
    for section, items in NAV:
        d.text(24, y, section, 10, FAINT, spacing="0.12em")
        y += 18
        for label in items:
            on = label == active
            if on:
                d.rect(14, y - 16, 204, 34, fill=BLUE, stroke=None, rx=6, op=0.10)
                d.rect(14, y - 16, 3, 34, fill=BLUE, stroke=None, rx=1.5)
            d.dot(34, y - 3, BLUE if on else FAINT, 3)
            d.text(48, y + 2, label, 12, TXT if on else DIM,
                   weight="bold" if on else None)
            if label == "Users":
                d.chip(178, y - 11, "soon", FAINT, 9, w=32)
            y += 38
        y += 16
    # config source card
    d.rect(16, 626, 200, 146, fill=PANEL2, stroke=BORDER, rx=8)
    d.text(30, 650, "CONFIG SOURCES", 9, FAINT, spacing="0.1em")
    for i, (name, state, color) in enumerate([
        ("arouter.toml", "read-only", DIM),
        ("web.toml", "2 writes", BLUE),
        ("admin.toml", "valid", GREEN),
    ]):
        yy = 678 + i * 26
        d.dot(32, yy - 4, color, 3)
        d.text(44, yy, name, 10, TXT if color != DIM else DIM, font=MONO)
        d.text(202, yy, state, 9, color, anchor="end")
    d.line(30, 752, 202, 752, BORDER)


def flow_rail(d, step, note):
    x, y, w, h = 264, 76, 984, 42
    d.rect(x, y, w, h, fill=PANEL, stroke=BORDER, rx=7)
    labels = ["Observe", "Diagnose", "Change", "Verify"]
    xs = [290, 454, 618, 782]
    d.line(xs[0] + 62, y + 21, xs[-1] + 28, y + 21, BORDER, 1)
    for i, (label, cx) in enumerate(zip(labels, xs), start=1):
        on = i == step
        done = i < step
        col = BLUE if on else (GREEN if done else FAINT)
        d.circle(cx, y + 21, 11, fill=col if on else BG, stroke=col if not on else None, width=1.5)
        d.text(cx, y + 25, str(i), 10, "#fff" if on else col, anchor="middle", weight="bold")
        d.text(cx + 18, y + 25, label, 11, TXT if on else DIM,
               weight="bold" if on else None)
    d.text(x + w - 16, y + 25, note, 10, AMBER if "review" in note.lower() else DIM,
           anchor="end")


def page_head(d, title, subtitle, buttons=None):
    d.text(264, 151, title, 24, TXT, weight="bold")
    d.text(264, 174, subtitle, 12, DIM)
    if buttons:
        right = 1248
        for label, kind, width in reversed(buttons):
            d.button(right - width, 132, width, label, kind)
            right -= width + 8


def shell(d, active, step, note, title, subtitle, buttons=None):
    d.rect(0, 0, W, H, fill=BG, stroke=None, rx=0)
    topbar(d)
    sidebar(d, active)
    flow_rail(d, step, note)
    page_head(d, title, subtitle, buttons)


def section(d, x, y, w, h, title=None, action=None, action_color=DIM):
    d.rect(x, y, w, h)
    if title:
        d.text(x + 16, y + 27, title, 12, TXT, weight="bold")
    if action:
        d.text(x + w - 16, y + 27, action, 10, action_color, anchor="end")


def muted(d, x, y, s, size=11):
    d.text(x, y, s, size, DIM)


def field(d, x, y, w, label, value, mono=False, hint=None):
    d.text(x, y, label, 10, DIM)
    d.rect(x, y + 8, w, 34, fill=BG, stroke=BORDER, rx=6)
    d.text(x + 12, y + 30, value, 12, TXT if value else FAINT, font=MONO if mono else SANS)
    if hint:
        d.text(x, y + 54, hint, 9, FAINT)


def status_row(d, x, y, w, dot, title, note, action, color=GREEN):
    d.dot(x + 10, y + 3, color, 4)
    d.text(x + 24, y + 7, title, 12, TXT, weight="bold")
    d.text(x + 24, y + 24, note, 10, DIM)
    d.text(x + w - 12, y + 7, action, 10, BLUE if action else DIM, anchor="end")
    d.line(x, y + 32, x + w, y + 32, BORDER, 1, op=0.55)


# ── 00 user flow ──────────────────────────────────────────────────────
def s00_flow():
    d = Doc(1280, 900)
    d.rect(0, 0, d.w, d.h, fill=BG, stroke=None, rx=0)
    d.raw('<polygon points="54,34 66,44 54,54 42,44" fill="#2DD4BF"/>')
    d.text(76, 50, "arouter", 21, TXT, weight="bold")
    d.chip(182, 35, "web console · rework", BLUE, 10)
    d.text(1196, 50, "mockup 00 · user flow", 11, FAINT, anchor="end")

    d.text(48, 112, "From first access to a confident change", 30, TXT, weight="bold")
    d.text(48, 140, "One flow for setup, diagnosis and configuration. Every screen answers: what is wrong, what will change, and how do I verify it?", 13, DIM)

    phases = [
        ("1", "First access", "Setup that cannot be missed", [
            "Open the console",
            "Create the admin account",
            "Session starts immediately",
            "Checklist: provider → model → key",
        ], BLUE),
        ("2", "Observe", "One page for the current truth", [
            "Serving / degraded banner",
            "Spend, errors, quota headroom",
            "“Needs attention” queue",
            "Recent changes and who made them",
        ], TEAL),
        ("3", "Diagnose & change", "Edit only what is understood", [
            "Inspect model / provider detail",
            "See config source and health",
            "Stage a change, preview impact",
            "Never save blindly",
        ], AMBER),
        ("4", "Verify", "Close the loop", [
            "Apply with a change summary",
            "Watch health + error rate",
            "Copy a sample request",
            "Audit entry in recent changes",
        ], GREEN),
    ]
    x = 48
    for i, (num, title, sub, items, color) in enumerate(phases):
        w = 276
        d.rect(x, 178, w, 226, fill=PANEL, stroke=BORDER, rx=12)
        d.circle(x + 34, 212, 17, fill=color, op=0.16)
        d.text(x + 34, 217, num, 14, color, anchor="middle", weight="bold")
        d.text(x + 60, 217, title, 16, TXT, weight="bold")
        d.text(x + 18, 250, sub, 11, color)
        for j, item in enumerate(items):
            d.dot(x + 24, 278 + j * 26, color, 2.5)
            d.text(x + 36, 282 + j * 26, item, 11, DIM)
        if i < len(phases) - 1:
            d.line(x + w + 8, 291, x + w + 28, 291, BORDER, 1.5)
            d.raw(f'<polygon points="{x+w+32},291 {x+w+24},286 {x+w+24},296" fill="{BORDER}"/>')
        x += w + 40

    # first-run lane
    d.text(48, 458, "FIRST-RUN PATH", 10, FAINT, spacing="0.12em")
    first = [
        ("Open URL", "no admin yet"),
        ("Create admin", "Argon2id · 0600"),
        ("Connect provider", "native or manual"),
        ("Pick models", "discovery + overlay"),
        ("Create first key", "scopes + budget"),
        ("Send test", "verify in overview"),
    ]
    x = 48
    for i, (title, note) in enumerate(first):
        w = 176
        planned = i >= 2
        d.rect(x, 476, w, 86, fill=PANEL2, stroke=None if planned else BORDER, rx=8)
        if planned:
            d.raw(f'<rect x="{x}" y="476" width="{w}" height="86" rx="8" fill="none" stroke="{BORDER}" stroke-dasharray="4 4"/>')
        d.text(x + 14, 501, title, 12, TXT, weight="bold")
        d.text(x + 14, 520, note, 9, DIM)
        d.text(x + w - 14, 545, "current" if not planned else "planned UI", 8,
               GREEN if not planned else AMBER, anchor="end")
        if i < len(first) - 1:
            d.text(x + w + 8, 522, "→", 14, FAINT)
        x += w + 20

    # day-2 lane
    d.text(48, 606, "DAY-2 PATH", 10, FAINT, spacing="0.12em")
    day = [
        ("Sign in", "session cookie"),
        ("Overview", "attention queue"),
        ("Open item", "model or provider"),
        ("Stage edit", "source + impact"),
        ("Review", "dry-run summary"),
        ("Apply", "same-origin write"),
        ("Verify", "health + sample"),
    ]
    x = 48
    for i, (title, note) in enumerate(day):
        w = 156
        d.rect(x, 624, w, 84, fill=PANEL, stroke=BORDER, rx=8)
        d.text(x + 13, 650, title, 12, TXT, weight="bold")
        d.text(x + 13, 669, note, 9, DIM)
        d.text(x + 13, 692, f"0{i+1}", 9, BLUE, font=MONO)
        if i < len(day) - 1:
            d.text(x + w + 5, 668, "→", 13, FAINT)
        x += w + 16

    # design rules
    d.rect(48, 750, 1184, 102, fill=PANEL2, stroke=BORDER, rx=10)
    d.text(66, 778, "DESIGN RULES", 10, FAINT, spacing="0.12em")
    rules = [
        ("Attention first", "the overview starts with what needs a decision"),
        ("Source visible", "manual / discovery / overlay is always labelled"),
        ("Stage, then apply", "preview impact before any write"),
        ("Secrets once", "plaintext key shown a single time"),
        ("No dead ends", "every empty state has the next action"),
    ]
    x = 66
    for i, (title, note) in enumerate(rules):
        d.text(x, 806, title, 11, TEAL, weight="bold")
        d.text(x, 826, note, 9, DIM)
        x += 230 if i < 4 else 0
        if i < 4:
            d.line(x - 16, 792, x - 16, 834, BORDER)
    d.save("00-user-flow.svg")


# ── 01 setup ──────────────────────────────────────────────────────────
def s01_setup():
    d = Doc()
    d.rect(0, 0, W, H, fill=BG, stroke=None, rx=0)
    d.raw('<polygon points="48,32 60,42 48,52 36,42" fill="#2DD4BF"/>')
    d.text(70, 48, "arouter", 20, TXT, weight="bold")
    d.chip(176, 33, "first access", AMBER, 10)
    d.text(1232, 48, "setup required · no admin on disk", 11, AMBER, anchor="end")

    # onboarding rail
    x, y, w = 48, 88, 1184
    d.rect(x, y, w, 46, fill=PANEL, stroke=BORDER, rx=8)
    steps = ["Admin account", "Connect provider", "Choose models", "First API key"]
    xs = [82, 360, 638, 916]
    d.line(xs[0] + 20, y + 23, xs[-1] + 20, y + 23, BORDER, 1)
    for i, (label, cx) in enumerate(zip(steps, xs), start=1):
        on = i == 1
        done = False
        col = BLUE if on else FAINT
        d.circle(cx, y + 23, 13, fill=col if on else BG, stroke=col if not on else None, width=1.5)
        d.text(cx, y + 27, str(i), 11, "#fff" if on else col, anchor="middle", weight="bold")
        d.text(cx + 22, y + 27, label, 12, TXT if on else DIM, weight="bold" if on else None)
        if i < 4:
            d.text(cx + 188, y + 27, "→", 13, FAINT)

    # left intro
    d.text(48, 194, "Set up arouter", 34, TXT, weight="bold")
    d.text(48, 224, "Four small steps. The router keeps serving the moment", 13, DIM)
    d.text(48, 244, "the admin account exists — no restart, no config edit.", 13, DIM)

    items = [
        ("1", "Admin account", "You are here. This account owns tokens, models and overrides.", BLUE, True),
        ("2", "Connect provider", "Native quota/balance for DeepSeek, z.ai and OpenRouter, or a manual window.", DIM, False),
        ("3", "Choose models", "Discovery proposes models; you choose grades, pools and weights.", DIM, False),
        ("4", "First API key", "Scoped key with a daily budget. The plaintext is shown once.", DIM, False),
    ]
    yy = 302
    for num, title, note, col, active in items:
        if active:
            d.rect(48, yy - 24, 400, 82, fill=BLUE, stroke=None, rx=8, op=0.08)
            d.rect(48, yy - 24, 3, 82, fill=BLUE, stroke=None, rx=1.5)
        d.circle(70, yy - 2, 12, fill=col if active else BG, stroke=col if not active else None, width=1.3)
        d.text(70, yy + 2, num, 10, "#fff" if active else col, anchor="middle", weight="bold")
        d.text(92, yy + 2, title, 13, TXT if active else DIM, weight="bold" if active else None)
        d.text(92, yy + 22, note, 10, DIM)
        yy += 94

    d.rect(48, 690, 400, 78, fill=PANEL, stroke=BORDER, rx=8)
    d.text(64, 716, "WHY THIS ORDER", 9, FAINT, spacing="0.1em")
    d.text(64, 738, "A key is useless without a model; a model is untrusted without", 10, DIM)
    d.text(64, 754, "a provider. Setup ends with a real test request.", 10, DIM)

    # form card
    x, y, w, h = 480, 160, 752, 608
    d.rect(x, y, w, h, fill=PANEL, stroke=BORDER, rx=12)
    d.text(x + 32, y + 44, "Create the admin account", 21, TXT, weight="bold")
    d.text(x + 32, y + 68, "This closes public setup permanently.", 11, DIM)
    d.chip(x + 586, y + 27, "step 1 of 4", BLUE, 10)

    field(d, x + 32, y + 104, 300, "USERNAME", "alice", mono=True)
    field(d, x + 360, y + 104, 328, "DISPLAY NAME (OPTIONAL)", "Alice")
    field(d, x + 32, y + 190, 300, "PASSWORD", "••••••••••••••••", mono=True,
          hint="12 characters minimum")
    field(d, x + 360, y + 190, 328, "CONFIRM PASSWORD", "••••••••••••••••", mono=True,
          hint="must match")
    # strength
    d.text(x + 32, y + 280, "STRENGTH", 9, FAINT, spacing="0.1em")
    d.bar(x + 94, y + 273, 180, 0.78, GREEN, 6)
    d.text(x + 286, y + 280, "strong", 10, GREEN)
    d.text(x + 360, y + 280, "No plaintext is stored — Argon2id only.", 10, DIM)

    # first-visitor warning
    d.rect(x + 32, y + 306, 688, 72, fill=AMBER, stroke=None, rx=8, op=0.10)
    d.rect(x + 32, y + 306, 3, 72, fill=AMBER, stroke=None, rx=1.5)
    d.text(x + 48, y + 330, "Finish this before exposing the port", 12, AMBER, weight="bold")
    d.text(x + 48, y + 350, "While no admin exists, the first visitor can claim the console. Bind to localhost, complete setup, then publish.", 10, DIM)
    d.text(x + 48, y + 366, "The console will show this warning until an admin exists.", 10, DIM)

    d.button(x + 32, y + 406, 196, "Create admin & continue", "primary", 42, 13)
    d.button(x + 240, y + 406, 120, "Read security notes", "secondary", 42, 11)
    d.text(x + 32, y + 472, "After this: connect a provider → choose models → create a key.", 11, DIM)

    d.line(x + 32, y + 500, x + 720, y + 500, BORDER)
    d.text(x + 32, y + 526, "WHAT GETS WRITTEN", 9, FAINT, spacing="0.1em")
    for i, (file, note) in enumerate([
        ("state_dir/admin.toml", "Argon2id hash · mode 0600 · atomic write"),
        ("state_dir/web.toml", "only when you change models or quota"),
    ]):
        yy = y + 548 + i * 24
        d.dot(x + 38, yy - 4, GREEN, 3)
        d.text(x + 50, yy, file, 10, TXT, font=MONO)
        d.text(x + 220, yy, note, 10, DIM)

    # right-side status
    d.text(1232, 792, "No claim link. No startup token. Setup is a browser flow.", 10, FAINT, anchor="end")
    d.save("01-setup.svg")


# ── 02 overview ───────────────────────────────────────────────────────
def s02_overview():
    d = Doc()
    shell(d, "Overview", 1, "healthy · last change 2m ago", "Overview",
          "thepotato · production · up 3d 4h · v0.1.0 · 274 models",
          [("Add model", "secondary", 100), ("Create key", "primary", 104)])

    cards = [
        ("REQUESTS · 24H", "18.4k", "+12% vs yesterday", BLUE, [4, 6, 5, 8, 7, 11, 10, 14, 13, 17], "12.1k"),
        ("5XX RATE", "0.4%", "82 responses · threshold 5%", GREEN, [3, 2, 4, 1, 2, 1, 3, 2, 1, 2], "0.2%"),
        ("SPEND TODAY", "$1.24", "of $5.00 budget · resets 00:00 UTC", AMBER, [2, 3, 3, 5, 4, 6, 7, 6, 8, 9], "$0.31/h"),
        ("MODELS", "274", "3 circuit open · 6 hidden", RED, [8, 8, 7, 7, 6, 7, 5, 5, 4, 4], "268 healthy"),
    ]
    x = 264
    for label, value, note, color, vals, foot in cards:
        d.rect(x, 194, 234, 108, fill=PANEL, stroke=BORDER, rx=9)
        d.text(x + 16, 219, label, 9, FAINT, spacing="0.08em")
        d.text(x + 16, 251, value, 26, TXT, weight="bold")
        d.text(x + 16, 272, note, 9, DIM)
        d.spark(x + 136, 226, 82, 30, vals, color)
        d.text(x + 16, 291, foot, 9, color)
        x += 250

    # attention queue
    section(d, 264, 320, 620, 238, "Needs attention", "3 items", AMBER)
    rows = [
        ("z.ai quota stale", "Last native sample was 18m ago. Routing may use an old window.", "Fix quota", AMBER),
        ("3 models circuit-open", "Failures in the last hour: or/gemini-3-pro, glm-4.7, local/llama-4.", "Inspect", RED),
        ("2 discovered models unpriced", "On-demand admission is refused until cost_in / cost_out exist.", "Review", BLUE),
    ]
    for i, (title, note, action, color) in enumerate(rows):
        status_row(d, 280, 356 + i * 62, 588, None, title, note, action, color)

    # traffic mix
    section(d, 900, 320, 348, 238, "Traffic mix · 24h", "by pool")
    d.donut(982, 424, 54, [(0.61, BLUE), (0.27, TEAL), (0.12, AMBER)])
    d.text(982, 429, "18.4k", 14, TXT, anchor="middle", weight="bold")
    d.text(982, 444, "requests", 8, DIM, anchor="middle")
    legend = [("quota", "61%", BLUE), ("local", "27%", TEAL), ("ondemand", "12%", AMBER)]
    for i, (name, pct, color) in enumerate(legend):
        yy = 382 + i * 28
        d.dot(1072, yy - 4, color, 4)
        d.text(1086, yy, name, 11, TXT)
        d.text(1214, yy, pct, 11, DIM, anchor="end")
    d.line(1066, 472, 1218, 472, BORDER)
    d.text(1066, 496, "spend", 9, DIM)
    d.text(1214, 496, "$1.24 today", 11, AMBER, anchor="end")
    d.text(1066, 516, "projected", 9, DIM)
    d.text(1214, 516, "$4.10 / day", 11, DIM, anchor="end")

    # recent changes
    section(d, 264, 576, 620, 204, "Recent changes", "view audit log", BLUE)
    heads = [("WHEN", 280), ("WHO", 400), ("CHANGE", 500), ("STATUS", 790)]
    for text, xx in heads:
        d.text(xx, 610, text, 8, FAINT, spacing="0.08em")
    changes = [
        ("2m", "alice", "disabled or/gemini-3-pro", "applied", GREEN),
        ("14m", "alice", "glm-5.3 weight 1.0 → 1.5", "applied", GREEN),
        ("1h", "alice", "z.ai 5h override 52% · ttl 24h", "superseded", AMBER),
        ("1d", "system", "discovered 12 models", "merged", DIM),
    ]
    for i, (when, who, change, status, color) in enumerate(changes):
        yy = 638 + i * 32
        d.text(280, yy, when, 10, DIM)
        d.text(400, yy, who, 10, TXT)
        d.text(500, yy, change, 10, TXT)
        d.text(790, yy, status, 9, color, anchor="end")
        d.line(280, yy + 12, 868, yy + 12, BORDER, 1, op=0.5)

    # quick start / verify
    section(d, 900, 576, 348, 204, "Close the loop", "sample request")
    d.text(916, 614, "The console is useful only if a change can be verified.", 10, DIM)
    steps = [("Health after apply", "ok", GREEN), ("5xx / 5m", "0.4%", GREEN), ("Last key used", "3m ago", BLUE)]
    for i, (label, value, color) in enumerate(steps):
        yy = 646 + i * 28
        d.dot(922, yy - 4, color, 3)
        d.text(936, yy, label, 10, DIM)
        d.text(1232, yy, value, 10, color, anchor="end")
    d.rect(916, 716, 316, 44, fill=BG, stroke=BORDER, rx=6)
    d.text(928, 736, "curl -s localhost:3929/v1/models \\", 9, DIM, font=MONO)
    d.text(928, 750, "  -H 'Authorization: Bearer ark_…'", 9, DIM, font=MONO)
    d.text(1232, 750, "copy", 9, BLUE, anchor="end")
    d.save("02-overview.svg")


# ── 03 models ─────────────────────────────────────────────────────────
def s03_models():
    d = Doc()
    shell(d, "Models", 3, "2 changes pending · review before apply", "Models",
          "274 active · 6 hidden · 3 circuit open · 2 missing price",
          [("Add model", "primary", 104)])

    # toolbar
    d.rect(264, 190, 300, 38, fill=BG, stroke=BORDER, rx=6)
    d.circle(282, 209, 5, stroke=DIM, width=1.4)
    d.line(286, 213, 291, 218, DIM, 1.4)
    d.text(300, 214, "Search id, provider or profile…", 10, FAINT)
    for i, (label, w) in enumerate([("All pools ▾", 92), ("All grades ▾", 94), ("Health ▾", 80), ("Source ▾", 80)]):
        d.rect(578 + i * 102, 190, w, 38, fill=PANEL, stroke=BORDER, rx=6)
        d.text(578 + i * 102 + 12, 214, label, 10, DIM)
    d.text(1240, 214, "Saved view: needs attention", 10, AMBER, anchor="end")

    # table
    d.rect(264, 242, 650, 508, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 272, "MODEL", 8, FAINT, spacing="0.08em")
    d.text(516, 272, "GRADE", 8, FAINT, spacing="0.08em")
    d.text(566, 272, "POOLS", 8, FAINT, spacing="0.08em")
    d.text(670, 272, "HEALTH", 8, FAINT, spacing="0.08em")
    d.text(764, 272, "SOURCE", 8, FAINT, spacing="0.08em")
    d.text(846, 272, "STATE", 8, FAINT, spacing="0.08em")
    d.line(280, 282, 898, 282, BORDER)

    rows = [
        ("glm-5.3", "A+", "quota", "ok", "config", "on", GREEN, False, False),
        ("glm-5.2", "A", "quota", "ok", "config", "on", GREEN, True, True),
        ("qwen3.8-flash", "B", "local", "ok", "discovered", "on", GREEN, False, False),
        ("or/claude-opus-4.6", "A+", "ondemand", "circuit", "overlay", "off", RED, False, False),
        ("deepseek-v3.2", "A", "quota", "ok", "discovered", "on", GREEN, False, False),
        ("ollama/qwen3-coder", "B", "local", "ok", "discovered", "on", GREEN, False, False),
        ("or/gemini-3-pro", "A", "ondemand", "missing price", "config", "on", AMBER, False, False),
        ("glm-4.7", "B", "quota", "stale", "config", "on", AMBER, True, False),
        ("zai/glm-5-air", "A", "quota", "ok", "overlay", "on", GREEN, False, False),
        ("local/llama-4", "B", "local", "ok", "discovered", "off", DIM, False, False),
    ]
    for i, (mid, grade, pool, health, source, state, color, selected, checked) in enumerate(rows):
        top = 292 + i * 38
        if selected:
            d.rect(272, top - 4, 634, 34, fill=BLUE, stroke=None, rx=5, op=0.08)
        d.checkbox(280, top + 2, checked)
        d.text(304, top + 15, mid, 11, TXT if state != "off" else DIM, font=MONO,
               weight="bold" if selected else None)
        gcol = GREEN if grade == "A+" else (AMBER if grade == "A" else BLUE)
        d.text(516, top + 15, grade, 10, gcol, weight="bold")
        d.text(566, top + 15, pool, 9, DIM)
        d.text(670, top + 15, health, 9, color)
        d.text(764, top + 15, source, 9, DIM)
        d.text(846, top + 15, state, 9, color if state != "on" else GREEN)
        d.text(890, top + 15, "›", 11, FAINT, anchor="end")

    d.text(280, 700, "Showing 10 of 274 · filters narrow the table, batch actions apply to selected rows.", 9, DIM)
    d.text(280, 720, "Structure fields stay config-managed; the console owns enable / weight / overlay additions.", 9, FAINT)

    # detail
    d.rect(930, 190, 318, 560, fill=PANEL, stroke=BORDER, rx=9)
    d.text(946, 218, "MODEL DETAIL", 8, FAINT, spacing="0.1em")
    d.text(1232, 218, "✕", 11, DIM, anchor="end")
    d.text(946, 246, "glm-5.2", 17, TXT, font=MONO, weight="bold")
    d.chip(946, 258, "A", AMBER, 9, w=26)
    d.chip(978, 258, "quota", BLUE, 9, w=46)
    d.chip(1030, 258, "healthy", GREEN, 9, w=54)
    d.text(946, 292, "z.ai · configured in arouter.toml", 9, DIM)

    def detail_section(y, label):
        d.text(946, y, label, 8, FAINT, spacing="0.1em")
        d.line(946, y + 8, 1232, y + 8, BORDER, 1, op=0.6)

    detail_section(318, "EFFECTIVE SETTINGS")
    for i, (k, v) in enumerate([("weight", "1.0"), ("context", "128k"), ("cost in / out", "$1.00 / $3.00"), ("profiles", "default, vip")]):
        yy = 340 + i * 20
        d.text(946, yy, k, 10, DIM)
        d.text(1232, yy, v, 10, TXT, anchor="end")

    detail_section(438, "ROUTING · 24H")
    for i, (k, v, color) in enumerate([("requests", "4.1k", TXT), ("p95 latency", "1.8s", TXT), ("fail ratio", "0.1%", GREEN), ("circuit", "closed", GREEN)]):
        yy = 460 + i * 20
        d.text(946, yy, k, 10, DIM)
        d.text(1232, yy, v, 10, color, anchor="end")

    detail_section(556, "OPERATIONAL CONTROL")
    d.text(946, 578, "Enabled", 10, TXT)
    d.switch(1190, 566, True)
    d.text(946, 610, "Weight", 10, TXT)
    d.rect(1060, 594, 88, 30, fill=BG, stroke=AMBER, rx=5)
    d.text(1076, 614, "1.5", 11, AMBER, font=MONO, weight="bold")
    d.text(1158, 614, "pending", 9, AMBER)
    d.text(946, 642, "Higher weight breaks ties within the same grade.", 9, FAINT)

    d.text(946, 674, "AFFECTED PROFILES", 8, FAINT, spacing="0.1em")
    d.chip(946, 684, "default", DIM, 9, w=52)
    d.chip(1004, 684, "vip", DIM, 9, w=34)
    d.text(946, 720, "Structure (base_url, grade, cost) is read-only here.", 9, FAINT)
    d.text(946, 736, "Change it in arouter.toml, then reload.", 9, FAINT)

    # apply bar
    d.rect(264, 760, 984, 44, fill=PANEL2, stroke=BORDER, rx=8)
    d.dot(282, 782, AMBER, 4)
    d.text(294, 786, "2 pending changes", 11, AMBER, weight="bold")
    d.text(420, 786, "glm-5.2 weight 1.0 → 1.5 · glm-4.7 disable", 10, DIM)
    d.button(962, 767, 84, "Discard", "ghost", 30, 10)
    d.button(1054, 767, 178, "Review impact & apply", "primary", 30, 10)
    d.save("03-models.svg")


# ── 03a add model ─────────────────────────────────────────────────────
def s03a_add_model():
    d = Doc()
    shell(d, "Models", 3, "overlay addition · dry-run before save", "Add model",
          "Add a model to the overlay · validate against the effective config",
          [("Cancel", "secondary", 84), ("Add model", "primary", 104)])

    # source picker
    d.rect(264, 190, 620, 54, fill=PANEL, stroke=BORDER, rx=8)
    for i, (label, on) in enumerate([("From discovery · 3 new", True), ("Manual endpoint", False), ("Duplicate existing", False)]):
        xx = 276 + i * 198
        if on:
            d.rect(xx, 200, 186, 34, fill=BLUE, stroke=None, rx=6, op=0.14)
        d.text(xx + 93, 222, label, 9, TXT if on else DIM, anchor="middle", weight="bold" if on else None)

    # form
    d.rect(264, 258, 620, 522, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 286, "Discovered model", 12, TXT, weight="bold")
    d.chip(816, 273, "not configured", GREEN, 8, w=68)
    d.text(280, 306, "openrouter/qwen3.7-max", 10, DIM, font=MONO)

    field(d, 280, 324, 588, "MODEL ID", "openrouter/qwen3.7-max", mono=True,
          hint="provider/model — the provider prefix is used for quota and metrics")
    field(d, 280, 386, 588, "BASE URL", "https://openrouter.ai/api/v1", mono=True,
          hint="http(s) only · trailing slash is removed")
    field(d, 280, 448, 286, "AUTH", "env:OPENROUTER_API_KEY", mono=True,
          hint="none · key:name · env:VAR")
    field(d, 582, 448, 286, "UPSTREAM NAME (OPTIONAL)", "qwen/qwen3.7-max", mono=True,
          hint="only when it differs from the id")

    d.text(280, 516, "GRADE", 10, DIM)
    d.rect(280, 524, 128, 34, fill=BG, stroke=BORDER, rx=6)
    d.text(292, 546, "A+", 12, GREEN, font=MONO, weight="bold")
    d.text(394, 546, "▾", 9, DIM)
    d.text(424, 516, "POOLS", 10, DIM)
    d.chip(424, 524, "quota", FAINT, 9, w=48)
    d.chip(478, 524, "local", FAINT, 9, w=44)
    d.chip(528, 524, "ondemand", BLUE, 9, w=64)
    d.text(600, 516, "CONTEXT", 10, DIM)
    d.rect(600, 524, 96, 34, fill=BG, stroke=BORDER, rx=6)
    d.text(612, 546, "200k", 11, TXT, font=MONO)
    d.text(704, 546, "tokens", 9, DIM)

    d.text(280, 590, "PRICE PER 1M TOKENS", 9, FAINT, spacing="0.08em")
    field(d, 280, 602, 180, "INPUT · USD", "0.60", mono=True)
    field(d, 474, 602, 180, "OUTPUT · USD", "2.40", mono=True)
    d.text(668, 634, "required for ondemand", 9, AMBER)
    d.text(668, 650, "unpriced ≠ free", 9, FAINT)

    d.text(280, 680, "CAPABILITIES", 9, FAINT, spacing="0.08em")
    d.chip(280, 690, "vision", GREEN, 9, w=52)
    d.chip(338, 690, "reasoning", GREEN, 9, w=68)
    d.chip(412, 690, "tools", FAINT, 9, w=44)
    d.text(468, 700, "auto-detected, overridable in config", 8, FAINT)

    d.line(280, 726, 868, 726, BORDER)
    d.text(280, 750, "Overlay additions are reversible. Existing models keep structure fields manual-only.", 9, DIM)

    # validation / review
    d.rect(900, 190, 348, 318, fill=PANEL, stroke=BORDER, rx=9)
    d.text(916, 220, "Validation", 13, TXT, weight="bold")
    d.chip(1160, 206, "6 checks", GREEN, 9, w=62)
    checks = [
        ("id has provider/model", "ok", GREEN),
        ("base_url is http(s)", "ok", GREEN),
        ("grade A+ is in the ladder", "ok", GREEN),
        ("ondemand has both prices", "ok", GREEN),
        ("no duplicate id", "ok", GREEN),
        ("endpoint reachability", "not tested", AMBER),
    ]
    for i, (label, state, color) in enumerate(checks):
        yy = 250 + i * 28
        d.dot(922, yy - 4, color, 3)
        d.text(934, yy, label, 10, TXT)
        d.text(1232, yy, state, 9, color, anchor="end")
    d.line(916, 416, 1232, 416, BORDER)
    d.text(916, 442, "Test endpoint", 10, BLUE)
    d.text(1232, 442, "planned · no quota spend", 8, FAINT, anchor="end")
    d.text(916, 466, "Dry-run applies to a clone; a rejected addition never", 9, DIM)
    d.text(916, 482, "changes the running snapshot.", 9, DIM)

    d.rect(900, 524, 348, 256, fill=PANEL, stroke=BORDER, rx=9)
    d.text(916, 552, "What will change", 13, TXT, weight="bold")
    d.chip(1160, 538, "overlay", BLUE, 8, w=58)
    for i, (k, v, color) in enumerate([
        ("source", "overlay", BLUE),
        ("profiles", "default, vip", TXT),
        ("grade", "A+", GREEN),
        ("cost", "$0.60 / $2.40", TXT),
        ("pool", "ondemand only", AMBER),
    ]):
        yy = 580 + i * 20
        d.text(916, yy, k, 9, DIM)
        d.text(1232, yy, v, 9, color, anchor="end")
    d.line(916, 680, 1232, 680, BORDER)
    d.text(916, 702, "Routing preview · ondemand", 9, FAINT, spacing="0.08em")
    d.dot(922, 722, GREEN, 3)
    d.text(934, 726, "openrouter/qwen3.7-max", 9, TXT, font=MONO)
    d.text(1232, 726, "new A+", 8, GREEN, anchor="end")
    d.dot(922, 746, DIM, 3)
    d.text(934, 750, "or/claude-opus-4.6", 9, DIM, font=MONO)
    d.text(1232, 750, "A+", 8, DIM, anchor="end")
    d.text(916, 770, "Cheapest-first ordering applies inside the grade.", 8, FAINT)

    d.save("03a-add-model.svg")


# ── 04 providers ──────────────────────────────────────────────────────
def s04_providers():
    d = Doc()
    shell(d, "Providers & quota", 3, "1 stale source · preview before override", "Providers & quota",
          "4 providers · 2 native pollers · 1 stale · 1 balance low",
          [("Refresh all", "secondary", 100), ("Add provider", "primary", 116)])

    cards = [
        ("z.ai", "stale · 18m", AMBER, "67%", "5h window", 0.67, AMBER, "weekly 41%", "native z.ai", "Routing: draining"),
        ("codex", "healthy", GREEN, "97%", "5h window", 0.97, RED, "weekly 81%", "native codex", "Routing: near limit"),
        ("openrouter", "healthy", GREEN, "$12.40", "wallet credit", 0.62, GREEN, "key limit 61%", "native openrouter", "Routing: eligible"),
        ("deepseek", "low balance", AMBER, "$8.12", "available balance", 0.34, AMBER, "granted $2.40", "native deepseek", "Routing: eligible"),
    ]
    x = 264
    for name, status, scol, metric, mlabel, pct, pcol, secondary, source, routing in cards:
        d.rect(x, 190, 234, 172, fill=PANEL, stroke=BORDER, rx=9)
        d.text(x + 16, 218, name, 15, TXT, weight="bold")
        d.chip(x + 146, 204, status, scol, 9, w=74)
        d.text(x + 16, 254, metric, 22, TXT, weight="bold")
        d.text(x + 16, 273, mlabel, 9, DIM)
        d.bar(x + 16, 284, 202, pct, pcol, 5)
        d.text(x + 16, 309, secondary, 9, DIM)
        d.text(x + 16, 329, source, 9, DIM)
        d.line(x + 16, 340, x + 218, 340, BORDER, 1, op=0.6)
        d.text(x + 16, 356, routing, 9, scol)
        x += 250

    # selected provider detail
    d.rect(264, 380, 620, 400, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 410, "z.ai · quota windows", 13, TXT, weight="bold")
    d.chip(704, 396, "native source · stale 18m", AMBER, 9, w=146)
    d.text(280, 434, "The newest signal wins. Overrides are synthetic and never beat fresher native data.", 10, DIM)

    # windows table
    d.text(280, 466, "WINDOW", 8, FAINT, spacing="0.08em")
    d.text(410, 466, "REMAINING", 8, FAINT, spacing="0.08em")
    d.text(570, 466, "RESET", 8, FAINT, spacing="0.08em")
    d.text(706, 466, "SOURCE", 8, FAINT, spacing="0.08em")
    d.line(280, 474, 868, 474, BORDER)
    for i, (win, pct, reset, src, color) in enumerate([
        ("5h", 0.67, "1h 17m", "native · 18m", AMBER),
        ("weekly", 0.41, "2d 4h", "native · 18m", GREEN),
        ("manual", 0.52, "—", "override · superseded", FAINT),
    ]):
        yy = 500 + i * 28
        d.text(280, yy, win, 10, TXT, weight="bold")
        d.bar(410, yy - 8, 110, pct, color, 5)
        d.text(530, yy, f"{int(pct*100)}%", 10, color)
        d.text(570, yy, reset, 9, DIM)
        d.text(706, yy, src, 9, DIM)
        d.line(280, yy + 10, 868, yy + 10, BORDER, 1, op=0.5)

    d.line(280, 598, 868, 598, BORDER)
    d.text(280, 622, "SET MANUAL OVERRIDE", 9, FAINT, spacing="0.1em")
    field(d, 280, 634, 184, "WINDOW", "5h", mono=True)
    field(d, 476, 634, 184, "REMAINING", "67%", mono=True)
    field(d, 672, 634, 196, "RESET IN", "1h 17m", mono=True)
    field(d, 280, 688, 184, "TTL", "24h", mono=True)
    field(d, 476, 688, 392, "NOTE (AUDIT)", "Operator saw z.ai dashboard at 67%", hint=None)
    d.text(280, 754, "Preview first · routing changes now · expires after 24h", 8, AMBER)
    d.button(608, 738, 126, "Preview impact", "secondary", 30, 9)
    d.button(742, 738, 126, "Apply override", "primary", 30, 9)

    # impact
    d.rect(900, 380, 348, 400, fill=PANEL, stroke=BORDER, rx=9)
    d.text(916, 410, "Routing impact", 13, TXT, weight="bold")
    d.chip(1114, 396, "preview only", FAINT, 9, w=80)
    d.text(916, 436, "If 5h is set to 67%", 10, DIM)
    d.text(916, 464, "3", 24, GREEN, weight="bold")
    d.text(942, 464, "models become eligible", 10, DIM)
    d.text(916, 492, "0", 24, RED, weight="bold")
    d.text(942, 492, "models excluded", 10, DIM)
    d.line(916, 510, 1232, 510, BORDER)
    d.text(916, 534, "CURRENTLY USING STALE DATA", 8, FAINT, spacing="0.08em")
    for i, (mid, state, color) in enumerate([
        ("zai/glm-5.3", "eligible", GREEN),
        ("zai/glm-5-air", "eligible", GREEN),
        ("zai/glm-5.2", "eligible", GREEN),
        ("zai/glm-4.7", "circuit open", RED),
    ]):
        yy = 558 + i * 26
        d.dot(922, yy - 4, color, 3)
        d.text(934, yy, mid, 9, TXT, font=MONO)
        d.text(1232, yy, state, 9, color, anchor="end")
    d.line(916, 672, 1232, 672, BORDER)
    d.text(916, 696, "SOURCE PRECEDENCE", 8, FAINT, spacing="0.08em")
    for i, (src, note, color) in enumerate([
        ("native (fresh)", "wins", GREEN),
        ("rate-limit headers", "traffic truth", BLUE),
        ("manual override", "synthetic", AMBER),
        ("none", "provider unknown", FAINT),
    ]):
        yy = 718 + i * 18
        d.text(916, yy, src, 9, color)
        d.text(1232, yy, note, 8, DIM, anchor="end")
    # CTAs
    d.button(916, 718, 0, "", "ghost", 0) if False else None
    d.save("04-providers.svg")


# ── 05 routing ────────────────────────────────────────────────────────
def s05_routing():
    d = Doc()
    shell(d, "Routing", 2, "diagnose · understand the candidate order", "Routing",
          "Deterministic candidate order · profile overrides · fallback is explicit",
          [("Simulate request", "primary", 124)])

    # profiles
    d.rect(264, 190, 270, 590, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 220, "PROFILES", 8, FAINT, spacing="0.1em")
    d.text(518, 220, "3", 8, DIM, anchor="end")
    profiles = [
        ("default", "quota → local → ondemand", "active", BLUE),
        ("vip", "ondemand → quota", "active", PURPLE),
        ("local", "local only", "active", TEAL),
        ("codex-drain", "quota → nothing", "paused", AMBER),
    ]
    for i, (name, route, state, color) in enumerate(profiles):
        yy = 238 + i * 86
        d.rect(280, yy, 238, 72, fill=PANEL2 if i else BLUE, stroke=BORDER, rx=8,
               op=None if i else 0.10)
        d.text(294, yy + 24, name, 13, TXT, weight="bold")
        d.chip(452, yy + 12, state, color, 9, w=50)
        d.text(294, yy + 44, route, 9, DIM)
        d.text(294, yy + 60, "grade floor A · fallback 1 below", 8, FAINT)
    d.rect(280, 594, 238, 60, fill="none", stroke=BORDER, rx=8)
    d.raw('<rect x="280" y="594" width="238" height="60" rx="8" fill="none" stroke="#30363D" stroke-dasharray="4 4"/>')
    d.text(294, 620, "+ New profile", 11, BLUE)
    d.text(294, 638, "copy a profile, then edit pools and fallback", 8, DIM)

    # profile detail
    d.rect(550, 190, 698, 250, fill=PANEL, stroke=BORDER, rx=9)
    d.text(566, 220, "default", 16, TXT, weight="bold")
    d.chip(642, 205, "config-managed", FAINT, 9, w=94)
    d.button(1110, 204, 122, "Duplicate profile", "secondary", 28, 9)
    d.text(566, 242, "The request asks for a grade; the profile decides which pools may answer.", 10, DIM)

    pools = [
        ("quota", "native subscription", "A+ .. B", BLUE, "first"),
        ("local", "resident engines", "A .. C", TEAL, "fallback"),
        ("ondemand", "paid, budget-capped", "A+ .. A", AMBER, "last"),
    ]
    x = 566
    for i, (pool, note, grades, color, role) in enumerate(pools):
        d.rect(x, 258, 206, 100, fill=PANEL2, stroke=BORDER, rx=8)
        d.dot(x + 18, 278, color, 4)
        d.text(x + 30, 282, pool, 14, TXT, weight="bold")
        d.text(x + 18, 304, note, 9, DIM)
        d.text(x + 18, 326, grades, 10, color, font=MONO)
        d.chip(x + 18, 334, role, color, 8, w=48)
        if i < 2:
            d.text(x + 216, 304, "→", 15, FAINT)
        x += 232

    d.line(566, 372, 1232, 372, BORDER)
    for i, (label, value, color) in enumerate([("Grade floor", "A", TXT), ("Fallback", "one grade below", AMBER), ("Cross-pool", "local only", TEAL)]):
        xx = 566 + i * 224
        d.text(xx, 396, label, 8, FAINT, spacing="0.08em")
        d.text(xx, 416, value, 11, color, weight="bold")

    # simulator
    d.rect(550, 454, 698, 326, fill=PANEL, stroke=BORDER, rx=9)
    d.text(566, 484, "Simulate a request", 13, TXT, weight="bold")
    d.chip(1120, 469, "read-only", FAINT, 9, w=62)
    d.text(566, 506, "Show the exact candidate order before routing a real caller.", 10, DIM)

    # controls
    controls = [("PROFILE", "default"), ("POOL", "quota"), ("GRADE", "A+ .. A"), ("CONTEXT", "32k")]
    for i, (label, value) in enumerate(controls):
        xx = 566 + i * 168
        d.text(xx, 536, label, 8, FAINT, spacing="0.08em")
        d.rect(xx, 544, 152, 32, fill=BG, stroke=BORDER, rx=5)
        d.text(xx + 10, 565, value, 10, TXT, font=MONO)
        d.text(xx + 142, 565, "▾", 9, DIM, anchor="end")

    # result rows
    d.line(566, 592, 1232, 592, BORDER)
    d.text(566, 614, "CANDIDATE ORDER", 8, FAINT, spacing="0.08em")
    d.text(1112, 614, "WHY", 8, FAINT, spacing="0.08em")
    candidates = [
        ("1", "glm-5.3", "A+", "quota", "selected", "best weight · healthy", GREEN),
        ("2", "glm-5.2", "A", "quota", "eligible", "fallback candidate", BLUE),
        ("3", "qwen3.8-flash", "B", "local", "skipped", "below grade floor", FAINT),
        ("4", "or/claude-opus-4.6", "A+", "ondemand", "excluded", "pool not allowed", RED),
        ("5", "glm-4.7", "B", "quota", "excluded", "circuit open", RED),
    ]
    for i, (num, mid, grade, pool, state, why, color) in enumerate(candidates):
        yy = 640 + i * 26
        if i == 0:
            d.rect(566, yy - 15, 666, 23, fill=GREEN, stroke=None, rx=4, op=0.07)
        d.text(566, yy, num, 9, FAINT, font=MONO)
        d.text(584, yy, mid, 10, TXT, font=MONO, weight="bold" if i == 0 else None)
        d.text(758, yy, grade, 9, DIM)
        d.text(804, yy, pool, 9, DIM)
        d.text(880, yy, state, 9, color)
        d.text(1112, yy, why, 9, DIM)
    d.text(566, 770, "Health, quota and token scope are filters — not preferences. The order never routes upward.", 9, FAINT)
    d.save("05-routing.svg")


# ── 06 API keys ───────────────────────────────────────────────────────
def s06_keys():
    d = Doc()
    shell(d, "API keys", 3, "secrets shown once · scope only narrows", "API keys",
          "3 active · $1.24 spent today · 1 key used in the last hour",
          [("Revoke all", "secondary", 92)])

    # one-time banner
    d.rect(264, 190, 984, 58, fill=GREEN, stroke=None, rx=8, op=0.10)
    d.rect(264, 190, 3, 58, fill=GREEN, stroke=None, rx=1.5)
    d.text(282, 214, "Key created — copy it now", 12, GREEN, weight="bold")
    d.text(282, 233, "The plaintext is shown once. Only a SHA-256 hash is stored.", 9, DIM)
    d.rect(632, 202, 396, 32, fill=BG, stroke=BORDER, rx=5)
    d.text(644, 222, "ark_live_9f4c…8d21", 11, TXT, font=MONO)
    d.chip(1040, 203, "Copy", BLUE, 9, w=44)
    d.text(1232, 232, "dismiss", 9, FAINT, anchor="end")

    # table
    d.rect(264, 264, 580, 516, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 294, "KEY", 8, FAINT, spacing="0.08em")
    d.text(430, 294, "SCOPE", 8, FAINT, spacing="0.08em")
    d.text(640, 294, "TODAY", 8, FAINT, spacing="0.08em")
    d.text(720, 294, "LAST USED", 8, FAINT, spacing="0.08em")
    d.text(840, 294, "", 8, FAINT)
    d.line(280, 304, 828, 304, BORDER)

    keys = [
        ("night-agent", "quota · A+..A · no ondemand", "$0.82", "3m ago", 0.41, GREEN),
        ("ci-bot", "local · B · ollama/*", "$0.00", "2h ago", 0.0, DIM),
        ("alice-laptop", "quota+ondemand · all grades", "$0.42", "yesterday", 0.84, AMBER),
        ("old-laptop", "revoked", "$0.00", "6d ago", 0.0, RED),
    ]
    for i, (name, scope, today, last, pct, color) in enumerate(keys):
        top = 316 + i * 108
        if i == 3:
            d.text(280, top + 20, name, 12, DIM, font=MONO, weight="bold")
            d.chip(430, top + 7, "revoked", RED, 9, w=50)
            d.text(840, top + 20, "—", 9, FAINT, anchor="end")
        else:
            d.text(280, top + 20, name, 12, TXT, font=MONO, weight="bold")
            d.text(280, top + 38, "ark_••••••••" + name[-2:], 8, FAINT, font=MONO)
            d.text(430, top + 20, scope, 9, DIM)
            d.text(640, top + 20, today, 10, TXT)
            d.text(720, top + 20, last, 9, DIM)
            d.text(840, top + 20, "revoke", 9, RED, anchor="end")
            d.bar(430, top + 34, 180, pct, color, 4)
            d.text(430, top + 48, f"{int(pct*100)}% of daily budget", 8, color)
        d.line(280, top + 78, 828, top + 78, BORDER, 1, op=0.5)

    d.text(280, 750, "Scopes only narrow access. A key can never exceed its pool, grade or budget.", 9, FAINT)

    # create form
    d.rect(860, 264, 388, 516, fill=PANEL, stroke=BORDER, rx=9)
    d.text(876, 294, "Create API key", 14, TXT, weight="bold")
    d.text(1232, 294, "cancel", 9, DIM, anchor="end")
    field(d, 876, 314, 356, "NAME", "night-agent", mono=True)
    d.text(876, 380, "POOLS", 9, FAINT, spacing="0.08em")
    d.chip(876, 390, "quota", BLUE, 9, w=48)
    d.chip(930, 390, "local", TEAL, 9, w=44)
    d.chip(980, 390, "ondemand", FAINT, 9, w=64)
    d.text(876, 428, "GRADES", 9, FAINT, spacing="0.08em")
    d.chip(876, 438, "A+", GREEN, 9, w=30)
    d.chip(912, 438, "A", AMBER, 9, w=26)
    d.chip(944, 438, "B", BLUE, 9, w=26)
    field(d, 876, 466, 356, "MODEL GLOB", "zai/*, deepseek/*")
    field(d, 876, 526, 170, "DAILY BUDGET", "$2.00")
    field(d, 1062, 526, 170, "EXPIRES", "30 days")
    d.rect(876, 594, 356, 82, fill=BG, stroke=BORDER, rx=6)
    d.text(888, 616, "PREVIEW", 8, FAINT, spacing="0.08em")
    d.text(888, 636, "Can reach 42 models in quota + local.", 9, GREEN)
    d.text(888, 652, "Cannot reach ondemand or models above A+.", 9, DIM)
    d.text(888, 668, "Budget stops at $2.00 UTC day.", 9, AMBER)
    d.button(876, 694, 356, "Create key", "primary", 38, 12)
    d.text(876, 750, "Shown once. Copy into the caller's secret store.", 8, FAINT)
    d.save("06-keys.svg")


# ── 07 settings ───────────────────────────────────────────────────────
def s07_settings():
    d = Doc()
    shell(d, "Settings", 2, "source visible · validate before reload", "Settings",
          "Runtime, configuration sources and security",
          [("Validate config", "secondary", 110), ("Reload from disk", "primary", 128)])

    tabs = [("General", False), ("Config sources", True), ("Security", False), ("State", False)]
    x = 264
    for label, on in tabs:
        w = 92 + len(label) * 5
        if on:
            d.rect(x, 190, w, 32, fill=BLUE, stroke=None, rx=6, op=0.12)
        d.text(x + w / 2, 211, label, 10, TXT if on else DIM, anchor="middle", weight="bold" if on else None)
        x += w + 8

    cards = [
        ("SERVER BIND", "0.0.0.0:3929", "nginx → 127.0.0.1:3929", BLUE),
        ("AUTH MODE", "required", "Bearer ark_… on /v1", GREEN),
        ("WEB SESSION", "12h", "HttpOnly · SameSite=Strict", TEAL),
        ("STATE DIR", "/var/lib/arouter", "admin.toml · web.toml", PURPLE),
    ]
    x = 264
    for label, value, note, color in cards:
        d.rect(x, 242, 234, 88, fill=PANEL, stroke=BORDER, rx=8)
        d.text(x + 14, 264, label, 8, FAINT, spacing="0.08em")
        d.text(x + 14, 291, value, 14, TXT, font=MONO, weight="bold")
        d.text(x + 14, 312, note, 8, DIM)
        d.dot(x + 220, 257, color, 3)
        x += 250

    # sources
    d.rect(264, 346, 620, 264, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 376, "Configuration sources", 13, TXT, weight="bold")
    d.text(868, 376, "top wins", 9, FAINT, anchor="end")
    d.text(280, 398, "Every effective value can be traced to one of these layers.", 9, DIM)
    heads = [("SOURCE", 280), ("PATH", 430), ("STATE", 704), ("ACTION", 840)]
    for text, xx in heads:
        d.text(xx, 426, text, 8, FAINT, spacing="0.08em")
    d.line(280, 436, 868, 436, BORDER)
    sources = [
        ("web.toml", "state/web.toml", "writable", "2 entries", BLUE, "Edit"),
        ("arouter.toml", "/etc/arouter/arouter.toml", "read-only", "valid", DIM, "View"),
        ("discovery", "runtime", "merged", "274 models", GREEN, "View"),
        ("environment", "arouter.env", "loaded", "6 overrides", DIM, "View"),
        ("admin.toml", "state/admin.toml", "valid", "hash only", GREEN, "View"),
    ]
    for i, (name, path, state, note, color, action) in enumerate(sources):
        yy = 460 + i * 28
        d.text(280, yy, name, 10, TXT, font=MONO, weight="bold" if i == 0 else None)
        d.text(430, yy, path, 9, DIM, font=MONO)
        d.chip(704, yy - 10, state, color, 8, w=58)
        d.text(772, yy, note, 9, DIM)
        d.text(840, yy, action, 9, BLUE)
        d.line(280, yy + 10, 868, yy + 10, BORDER, 1, op=0.45)
    d.text(280, 596, "Only web.toml is writable from the console. Structure and credentials stay manual.", 9, AMBER)

    # effective config
    d.rect(900, 346, 348, 264, fill=PANEL, stroke=BORDER, rx=9)
    d.text(916, 376, "Effective config", 13, TXT, weight="bold")
    d.chip(1160, 362, "preview", FAINT, 9, w=58)
    d.rect(916, 394, 316, 150, fill=BG, stroke=BORDER, rx=6)
    lines = [
        "[server]",
        'bind = "0.0.0.0:3929"',
        "",
        "[auth]",
        "require = true",
        "",
        "[[models]]",
        'id = "zai/glm-5.3"',
        "… 274 models",
    ]
    for i, line in enumerate(lines):
        col = TEAL if line.startswith("[") else (DIM if not line else TXT)
        d.text(928, 414 + i * 15, line, 9, col, font=MONO)
    d.button(916, 558, 150, "Download TOML", "secondary", 30, 9)
    d.button(1074, 558, 158, "Copy effective", "secondary", 30, 9)
    d.text(916, 600, "Generated from manual + discovery + overlay.", 8, FAINT)

    # bottom left apply
    d.rect(264, 626, 620, 154, fill=PANEL, stroke=BORDER, rx=9)
    d.text(280, 654, "Apply & maintenance", 13, TXT, weight="bold")
    d.text(280, 678, "No pending config changes. The running snapshot is valid.", 10, DIM)
    d.dot(286, 700, GREEN, 3)
    d.text(298, 704, "last reload 2m ago · 274 models · 0 load errors", 9, DIM)
    d.button(280, 726, 136, "Validate config", "secondary", 32, 9)
    d.button(424, 726, 150, "Reload from disk", "secondary", 32, 9)
    d.text(280, 770, "Reload rebuilds the snapshot; rejected config leaves the old one serving.", 8, FAINT)

    # admin
    d.rect(900, 626, 348, 154, fill=PANEL, stroke=BORDER, rx=9)
    d.text(916, 654, "Admin account", 13, TXT, weight="bold")
    d.circle(928, 682, 12, fill=TEAL, op=0.18)
    d.text(928, 686, "A", 9, TEAL, anchor="middle", weight="bold")
    d.text(946, 680, "alice", 11, TXT, weight="bold")
    d.text(946, 696, "role: admin · created 12d ago", 8, DIM)
    d.text(1232, 680, "change password", 8, BLUE, anchor="end")
    d.text(1232, 696, "sign out all sessions", 8, DIM, anchor="end")
    d.line(916, 714, 1232, 714, BORDER)
    d.text(916, 736, "Argon2id hash in state/admin.toml · mode 0600", 8, DIM)
    d.text(916, 754, "Setup closes permanently after the first admin.", 8, AMBER)
    d.save("07-settings.svg")


# ── 08 mobile ─────────────────────────────────────────────────────────
def s08_mobile():
    d = Doc()
    d.rect(0, 0, W, H, fill=BG, stroke=None, rx=0)
    d.raw('<polygon points="48,32 60,42 48,52 36,42" fill="#2DD4BF"/>')
    d.text(70, 48, "arouter", 20, TXT, weight="bold")
    d.chip(176, 33, "responsive", TEAL, 10)
    d.text(1232, 48, "same flow · thumb-first actions", 11, FAINT, anchor="end")
    d.text(48, 104, "The console on a phone", 26, TXT, weight="bold")
    d.text(48, 128, "Overview, model switches and quota overrides remain usable without a laptop.", 12, DIM)

    phones = [
        (100, "Overview", "status first"),
        (490, "Models", "stage a change"),
        (880, "Quota override", "preview impact"),
    ]
    for px, title, note in phones:
        x, y, w, h = px, 152, 300, 590
        d.rect(x, y, w, h, fill=PANEL, stroke=BORDER, rx=28)
        d.rect(x + 8, y + 8, w - 16, h - 16, fill=BG, stroke=None, rx=22)
        d.rect(x + 118, y + 12, 64, 5, fill=BORDER, stroke=None, rx=2.5)
        # phone top
        d.raw(f'<polygon points="{x+24},{y+36} {x+32},{y+42} {x+24},{y+48} {x+16},{y+42}" fill="#2DD4BF"/>')
        d.text(x + 38, y + 47, "arouter", 12, TXT, weight="bold")
        d.text(x + w - 18, y + 47, "alice", 9, DIM, anchor="end")
        d.line(x + 16, y + 58, x + w - 16, y + 58, BORDER)

        if title == "Overview":
            d.text(x + 16, y + 88, "Overview", 18, TXT, weight="bold")
            d.chip(x + 16, y + 98, "● healthy", GREEN, 8, w=58)
            for i, (label, value, color) in enumerate([("requests", "18.4k", BLUE), ("5xx", "0.4%", GREEN), ("spend", "$1.24", AMBER), ("models", "274", RED)]):
                cx = x + 16 + (i % 2) * 134
                cy = y + 124 + (i // 2) * 70
                d.rect(cx, cy, 118, 58, fill=PANEL, stroke=BORDER, rx=7)
                d.text(cx + 10, cy + 18, label, 7, FAINT, spacing="0.06em")
                d.text(cx + 10, cy + 40, value, 15, TXT, weight="bold")
                d.dot(cx + 102, cy + 12, color, 3)
            d.text(x + 16, y + 286, "Needs attention", 11, TXT, weight="bold")
            for i, (t, c) in enumerate([("z.ai quota stale", AMBER), ("3 circuits open", RED), ("2 models unpriced", BLUE)]):
                yy = y + 306 + i * 48
                d.rect(x + 16, yy, 268, 38, fill=PANEL, stroke=BORDER, rx=6)
                d.dot(x + 28, yy + 19, c, 3)
                d.text(x + 40, yy + 17, t, 9, TXT)
                d.text(x + 40, yy + 30, "review →", 8, c)
            d.rect(x + 16, y + 462, 268, 76, fill=PANEL, stroke=BORDER, rx=7)
            d.text(x + 28, y + 486, "Traffic mix", 9, DIM)
            d.bar(x + 28, y + 500, 110, 0.61, BLUE, 7)
            d.bar(x + 28, y + 510, 110, 0.27, TEAL, 7)
            d.text(x + 160, y + 508, "quota 61%", 8, DIM)

        elif title == "Models":
            d.text(x + 16, y + 88, "Models", 18, TXT, weight="bold")
            d.chip(x + 16, y + 98, "2 pending", AMBER, 8, w=62)
            d.rect(x + 16, y + 120, 268, 30, fill=PANEL, stroke=BORDER, rx=5)
            d.text(x + 28, y + 140, "Search models…", 8, FAINT)
            for i, (m, g, state, color) in enumerate([("glm-5.3", "A+", "on", GREEN), ("glm-5.2", "A", "on", GREEN), ("or/claude", "A+", "off", RED), ("qwen3.8", "B", "on", GREEN), ("glm-4.7", "B", "stale", AMBER)]):
                yy = y + 162 + i * 48
                d.rect(x + 16, yy, 268, 40, fill=PANEL, stroke=BORDER, rx=6)
                d.text(x + 28, yy + 17, m, 9, TXT, font=MONO, weight="bold")
                d.text(x + 28, yy + 30, g + " · quota", 8, DIM)
                d.switch(x + 224, yy + 9, state == "on", 40)
                d.text(x + 192, yy + 30, state, 7, color, anchor="end")
            d.rect(x + 16, y + 424, 268, 56, fill=PANEL2, stroke=AMBER, rx=7)
            d.text(x + 28, y + 448, "2 pending changes", 9, AMBER, weight="bold")
            d.text(x + 28, y + 464, "review impact before apply", 8, DIM)
            d.rect(x + 158, y + 438, 112, 28, fill=PRIMARY, stroke=None, rx=5)
            d.text(x + 214, y + 457, "Review", 8, "#fff", anchor="middle", weight="bold")

        else:
            d.text(x + 16, y + 88, "z.ai quota", 18, TXT, weight="bold")
            d.chip(x + 16, y + 98, "stale 18m", AMBER, 8, w=62)
            d.rect(x + 16, y + 120, 268, 92, fill=PANEL, stroke=BORDER, rx=7)
            d.text(x + 28, y + 146, "5h window", 9, DIM)
            d.text(x + 28, y + 174, "67%", 22, TXT, weight="bold")
            d.bar(x + 96, y + 162, 170, 0.67, AMBER, 6)
            d.text(x + 28, y + 198, "reset 1h 17m · native 18m ago", 8, DIM)
            d.text(x + 16, y + 240, "Set manual override", 10, TXT, weight="bold")
            for i, (label, value) in enumerate([("window", "5h"), ("remaining", "67%"), ("reset", "1h 17m"), ("ttl", "24h")]):
                yy = y + 256 + i * 52
                d.text(x + 16, yy, label.upper(), 7, FAINT, spacing="0.08em")
                d.rect(x + 16, yy + 8, 268, 30, fill=PANEL, stroke=BORDER, rx=5)
                d.text(x + 28, yy + 28, value, 9, TXT, font=MONO)
            d.rect(x + 16, y + 470, 268, 62, fill=AMBER, stroke=None, rx=6, op=0.08)
            d.text(x + 28, y + 492, "Preview: 3 models eligible", 8, GREEN)
            d.text(x + 28, y + 508, "Native fresher data still wins.", 8, DIM)
            d.rect(x + 16, y + 546, 268, 34, fill=PRIMARY, stroke=None, rx=6)
            d.text(x + 150, y + 568, "Preview & apply", 9, "#fff", anchor="middle", weight="bold")

        d.text(x + w / 2, y + h + 22, title, 11, TXT, anchor="middle", weight="bold")
        d.text(x + w / 2, y + h + 38, note, 9, DIM, anchor="middle")

    d.text(48, 804, "Responsive rules: keep the attention queue, collapse tables into cards, keep apply bars sticky.", 9, FAINT)
    d.save("08-mobile.svg")


# ── gallery ───────────────────────────────────────────────────────────
SCREENS = [
    ("00-user-flow.svg", "User flow", "Setup → observe → diagnose/change → verify. Includes the first-run and day-2 paths."),
    ("01-setup.svg", "First access", "Admin creation with an explicit warning while the console is unclaimed."),
    ("02-overview.svg", "Overview", "Health, spend, quota headroom, attention queue, recent changes and verification."),
    ("03-models.svg", "Models", "Search and filter, inspect, stage enable/weight changes, review impact, apply."),
    ("03a-add-model.svg", "Add model", "Discovery or manual addition with validation, cost guard and routing preview."),
    ("04-providers.svg", "Providers & quota", "Native balance/windows, staleness, manual override and routing impact preview."),
    ("05-routing.svg", "Routing", "Profiles, pool order, fallback policy and a read-only request simulator."),
    ("06-keys.svg", "API keys", "Scoped keys, one-time secret, budget, last use and revocation."),
    ("07-settings.svg", "Settings", "Config sources, effective config, validation/reload and admin security."),
    ("08-mobile.svg", "Mobile", "The same flow with thumb-first controls and sticky apply bars."),
]


def gallery():
    cards = "\n".join(
        f'''    <section class="shot">
      <div class="meta"><h2>{esc(title)}</h2><p>{esc(note)}</p><code>web-v2/{esc(name)}</code></div>
      <a href="{esc(name)}"><img src="{esc(name)}" alt="{esc(title)} mockup"></a>
    </section>''' for name, title, note in SCREENS)

    html = f'''<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>arouter web console — rework mockups</title>
<style>
:root {{ color-scheme: dark; --bg:#0D1117; --panel:#161B22; --line:#30363D; --txt:#E6EDF3; --dim:#8B949E; --blue:#58A6FF; }}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--bg); color:var(--txt); font:14px/1.55 -apple-system,'Segoe UI',sans-serif; }}
header {{ padding:48px 6vw 24px; border-bottom:1px solid var(--line); }}
header h1 {{ margin:0 0 8px; font-size:30px; }}
header p {{ margin:0; color:var(--dim); max-width:78ch; }}
header a {{ color:var(--blue); }}
nav {{ padding:20px 6vw; display:flex; gap:8px; flex-wrap:wrap; border-bottom:1px solid var(--line); }}
nav a {{ color:var(--dim); text-decoration:none; border:1px solid var(--line); padding:5px 10px; border-radius:5px; font-size:12px; }}
nav a:hover {{ color:var(--txt); border-color:var(--blue); }}
main {{ padding:24px 6vw 80px; }}
.shot {{ margin:0 0 56px; }}
.meta {{ display:flex; gap:16px; align-items:baseline; flex-wrap:wrap; margin:0 0 10px; }}
.meta h2 {{ margin:0; font-size:18px; }}
.meta p {{ margin:0; color:var(--dim); flex:1; min-width:280px; }}
.meta code {{ color:#6E7681; font-size:11px; }}
.shot img {{ display:block; width:100%; height:auto; border:1px solid var(--line); border-radius:12px; background:#0D1117; }}
</style></head>
<body>
<header>
  <h1>arouter web console — rework mockups</h1>
  <p>A configuration surface rather than a status page. The overview starts with what needs a decision; every change is staged, sourced and previewed before it is applied. Dashed items in the flow are planned UI, not current capability.</p>
  <p style="margin-top:10px"><a href="README.md">Feature list and implementation notes</a> · <a href="../../../docs/web-console-rework.md">product brief</a></p>
</header>
<nav>{"".join(f'<a href="{esc(n)}">{esc(t)}</a>' for n,t,_ in SCREENS)}</nav>
<main>
{cards}
</main>
</body></html>
'''
    (HERE / "index.html").write_text(html)


def main():
    HERE.mkdir(parents=True, exist_ok=True)
    s00_flow()
    s01_setup()
    s02_overview()
    s03_models()
    s03a_add_model()
    s04_providers()
    s05_routing()
    s06_keys()
    s07_settings()
    s08_mobile()
    gallery()
    print(f"wrote {len(SCREENS)} mockups + index.html to {HERE}")


if __name__ == "__main__":
    main()
