//! E5: htop-style monitor TUI. Polls `/status.json` once a second and
//! renders providers (LED panel), models (quota bar, tok/s EMA, 24h
//! sparkline, spend) and callers. Pure transforms are unit-tested below.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;
use serde::Deserialize;

// ─────────────────────────── data model ───────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub bind: String,
    #[serde(default)]
    pub auth_required: bool,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default)]
    pub models: Vec<ModelRow>,
    #[serde(default)]
    pub health: HashMap<String, HealthRow>,
    #[serde(default)]
    pub quota: Quota,
    #[serde(default)]
    pub usage_by_model: HashMap<String, Usage>,
    #[serde(default)]
    pub callers: HashMap<String, Usage>,
    #[serde(default)]
    pub requests_total: u64,
    #[serde(default)]
    pub hourly_by_model: HashMap<String, Vec<u64>>,
    #[serde(default)]
    pub hourly_by_caller: HashMap<String, Vec<u64>>,
    #[serde(default)]
    pub local_engines: LocalEnginesStatus,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ModelRow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub grade: String,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub weight: f64,
    #[serde(default)]
    pub provenance: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct HealthRow {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub secs_left: u64,
    #[serde(default)]
    pub fail_ratio_1h: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub providers: HashMap<String, Provider>,
    #[serde(default)]
    pub provider_map: HashMap<String, String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Provider {
    #[serde(default)]
    pub availability: Option<f64>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct QuotaWindow {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub pct: f64,
    #[serde(default)]
    pub reset_in_secs: Option<i64>,
}

// E8: local engine residency (see src/localeng.rs)
#[derive(Debug, Default, Clone, Deserialize)]
pub struct LocalEnginesStatus {
    #[serde(default)]
    pub stale: bool,
    #[serde(default)]
    pub fetched_unix: u64,
    #[serde(default)]
    pub engines: Vec<EngineRow>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct EngineRow {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub loaded: HashMap<String, LoadedInfo>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct LoadedInfo {
    #[serde(default)]
    pub size_vram: u64,
    #[serde(default)]
    pub expires_unix: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub ok: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub last_ts_unix: u64,
}

// ─────────────────────────── pure transforms ───────────────────────────

pub const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 24-hour sparkline: `▁▂▃▄▅▆▇█` scaled to the series max; zeros render as
/// spaces so bars stay readable.
pub fn sparkline(series: &[u64]) -> String {
    let max = series.iter().copied().max().unwrap_or(0);
    series
        .iter()
        .map(|&v| {
            if max == 0 || v == 0 {
                ' '
            } else {
                let idx = (((v as f64 / max as f64) * 8.0).ceil() as usize).clamp(1, 8) - 1;
                SPARK_CHARS[idx]
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Led {
    Green,
    Amber,
    Red,
    Gray,
}

/// LED classification for a quota panel (docs/routing.md availability):
/// unknown → gray; >0.9 and clean → green; hard zero → red; anything else
/// (mid range or warning notes / fetch error) → amber.
pub fn led(availability: Option<f64>, has_notes: bool, error: bool) -> Led {
    match availability {
        None => Led::Gray,
        Some(a) => {
            if error {
                Led::Amber
            } else if a <= 0.0 {
                Led::Red
            } else if a > 0.9 && !has_notes {
                Led::Green
            } else {
                Led::Amber
            }
        }
    }
}

impl Led {
    pub fn color(self) -> Color {
        match self {
            Led::Green => Color::Green,
            Led::Amber => Color::Yellow,
            Led::Red => Color::Red,
            Led::Gray => Color::DarkGray,
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            Led::Green => "●",
            Led::Amber => "◐",
            Led::Red => "✕",
            Led::Gray => "○",
        }
    }
}

/// Compact countdown: `1h23m`, `4m05s`, `45s`; `—` when unknown, `now`
/// when already due.
pub fn fmt_countdown(secs: Option<i64>) -> String {
    match secs {
        None => "—".into(),
        Some(s) if s <= 0 => "now".into(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        Some(s) => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// host:port of an engine url (strip scheme and path).
pub fn host_of(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}

/// VRAM bytes → "8.1G" / "512M" ("—" for 0/unknown).
pub fn fmt_bytes(bytes: u64) -> String {
    const M: f64 = 1024.0 * 1024.0;
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    match bytes {
        0 => "—".into(),
        b if (b as f64) >= G => format!("{:.1}G", b as f64 / G),
        b if (b as f64) >= M => format!("{:.0}M", b as f64 / M),
        b => format!("{b}"),
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn fmt_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        format!("{:.1}M", n as f64 / 1e6)
    }
}

pub fn fmt_usd(v: f64) -> String {
    if v == 0.0 {
        "—".into()
    } else if v < 0.01 {
        format!("{v:.6}")
    } else {
        format!("{v:.2}")
    }
}

/// Exponential moving average update (alpha = 0.3 per E5 design).
pub fn ema(prev: f64, sample: f64) -> f64 {
    prev + 0.3 * (sample - prev)
}

/// Does a model pass the active filters?
pub fn row_matches(m: &ModelRow, pool: Option<&str>, grade: Option<&str>, search: &str) -> bool {
    if let Some(p) = pool {
        if !m.pools.iter().any(|mp| mp == p) {
            return false;
        }
    }
    if let Some(g) = grade {
        if m.grade != g {
            return false;
        }
    }
    if !search.is_empty() && !m.id.to_lowercase().contains(&search.to_lowercase()) {
        return false;
    }
    true
}

/// A flattened, display-ready model row (built per poll).
#[derive(Debug, Clone, Default)]
pub struct DispRow {
    pub grade: String,
    pub id: String,
    pub pools: String,
    pub health: String,
    pub health_state: String,
    pub quota_pct: Option<f64>,
    pub reset_in: Option<i64>,
    pub tok_s: f64,
    pub spark: String,
    pub cost_usd: f64,
    pub weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Grade,
    Spend,
    ResetsSoon,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Grade => "grade",
            SortMode::Spend => "spend",
            SortMode::ResetsSoon => "resets",
        }
    }
}

/// Stable sort of display rows by the active mode (grade/spend/resets).
pub fn sort_rows(rows: &mut [DispRow], mode: SortMode) {
    match mode {
        SortMode::Grade => rows.sort_by(|a, b| {
            grade_rank(&a.grade)
                .cmp(&grade_rank(&b.grade))
                .then_with(|| a.id.cmp(&b.id))
        }),
        SortMode::Spend => rows.sort_by(|a, b| {
            b.cost_usd
                .partial_cmp(&a.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        }),
        SortMode::ResetsSoon => rows.sort_by(|a, b| {
            // rows with a countdown first, soonest at top
            let ka = a.reset_in.unwrap_or(i64::MAX);
            let kb = b.reset_in.unwrap_or(i64::MAX);
            ka.cmp(&kb).then_with(|| a.id.cmp(&b.id))
        }),
    }
}

fn grade_rank(g: &str) -> usize {
    // ladder order A+ < A < B < C < …; unknown grades sort last
    match g {
        "A+" => 0,
        "A" => 1,
        "B" => 2,
        "C" => 3,
        _ => 9,
    }
}

/// Which quota panel (if any) backs this model: explicit provider_map
/// alias first, then a provider named exactly like the id prefix.
pub fn panel_for<'a>(
    id: &str,
    map: &'a HashMap<String, String>,
    providers: &'a HashMap<String, Provider>,
) -> Option<&'a Provider> {
    let prefix = id.split('/').next().unwrap_or(id);
    if let Some(panel) = map.get(prefix) {
        return providers.get(panel);
    }
    providers.get(prefix)
}

/// Worst window (lowest pct) and its reset countdown for a provider.
/// `/status.json` reports window pct on a 0–100 scale; we normalize to
/// 0–1 for bars and display.
pub fn worst_window(p: &Provider) -> (Option<f64>, Option<i64>) {
    let pct = p
        .windows
        .iter()
        .map(|w| w.pct / 100.0)
        .fold(f64::INFINITY, f64::min)
        .clamp(0.0, 1.0);
    let pct = if pct.is_finite() { Some(pct) } else { None };
    let reset = p
        .windows
        .iter()
        .filter_map(|w| w.reset_in_secs)
        .filter(|&s| s > 0)
        .min();
    (pct, reset)
}

/// A 10-cell bar: `▓▓▓▓▓░░░░░` at the given fraction.
pub fn bar(frac: f64, width: usize) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    let mut s = String::new();
    for i in 0..width {
        s.push(if i < filled { '▓' } else { '░' });
    }
    s
}

// ─────────────────────────── app state ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Models,
    Callers,
    Engines,
    Help,
}

pub struct Mon {
    pub url: String,
    pub page: Page,
    pub status: Option<Status>,
    pub rows: Vec<DispRow>,
    pub led_rows: Vec<(String, Led, String, String, bool)>, // name, led, pct, reset, draining
    pub tok_ema: HashMap<String, f64>,
    pub prev_totals: HashMap<String, u64>,
    pub prev_requests: u64,
    pub last_poll: Instant,
    pub last_ok: Option<Instant>,
    pub error: Option<String>,
    pub pool_filter: Option<&'static str>,
    pub grade_filter: Option<String>,
    pub grades: Vec<String>,
    pub profiles: Vec<String>,
    pub profile_filter: usize, // 0 = all
    pub sort: SortMode,
    pub search: String,
    pub search_mode: bool,
    pub active: bool,
}

pub const POOL_KEYS: [&str; 3] = ["quota", "local", "ondemand"];

impl Mon {
    pub fn new(url: String) -> Self {
        Self {
            url,
            page: Page::Models,
            status: None,
            rows: Vec::new(),
            led_rows: Vec::new(),
            tok_ema: HashMap::new(),
            prev_totals: HashMap::new(),
            prev_requests: 0,
            last_poll: Instant::now(),
            last_ok: None,
            error: None,
            pool_filter: None,
            grade_filter: None,
            grades: Vec::new(),
            profiles: Vec::new(),
            profile_filter: 0,
            sort: SortMode::Grade,
            search: String::new(),
            search_mode: false,
            active: false,
        }
    }

    /// One poll cycle: fetch, merge deltas (EMA), rebuild display rows.
    pub async fn poll(&mut self, http: &reqwest::Client) {
        let dt = self.last_poll.elapsed().as_secs_f64().max(0.001);
        self.last_poll = Instant::now();
        match http
            .get(&self.url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.json::<Status>().await {
                Ok(st) => {
                    self.error = None;
                    self.last_ok = Some(Instant::now());
                    self.absorb(st, dt);
                }
                Err(e) => self.error = Some(format!("bad status payload: {e}")),
            },
            Ok(r) => self.error = Some(format!("daemon answered {}", r.status())),
            Err(e) => self.error = Some(format!("daemon unreachable: {e}")),
        }
    }

    fn absorb(&mut self, st: Status, dt: f64) {
        // tok/s EMA from cumulative token deltas between polls
        for (id, u) in &st.usage_by_model {
            let total = u.tokens_in + u.tokens_out;
            let sample = match self.prev_totals.get(id) {
                Some(&p) if total >= p => (total - p) as f64 / dt,
                _ => 0.0,
            };
            let prev = self.tok_ema.get(id).copied().unwrap_or(0.0);
            let next = ema(prev, sample);
            self.tok_ema.insert(id.clone(), next);
            self.prev_totals.insert(id.clone(), total);
        }
        self.active = st.requests_total > self.prev_requests;
        self.prev_requests = st.requests_total;

        // filter option lists (grades present, sorted by ladder rank)
        let mut grades: Vec<String> = st.models.iter().map(|m| m.grade.clone()).collect();
        grades.sort_by_key(|g| (grade_rank(g), g.clone()));
        grades.dedup();
        self.grades = grades;
        if let Some(p) = &self.grade_filter {
            if !self.grades.contains(p) {
                self.grade_filter = None;
            }
        }
        let mut profiles: Vec<String> = st.models.iter().flat_map(|m| m.profiles.clone()).collect();
        profiles.sort();
        profiles.dedup();
        self.profiles = profiles;

        // LED panel: one segment per quota provider
        self.led_rows = {
            let mut v: Vec<_> = st
                .quota
                .providers
                .iter()
                .map(|(name, p)| {
                    let draining = p.notes.iter().any(|n| n.starts_with("draining"));
                    // drain is an opportunity, not a warning — only other
                    // notes force the LED amber
                    let has_notes = p.notes.iter().any(|n| !n.starts_with("draining"));
                    let err = p.error.is_some() || p.stale;
                    let l = led(p.availability, has_notes, err);
                    let (pct, reset) = worst_window(p);
                    let pct_s = pct
                        .map(|x| format!("{:>3.0}%", x * 100.0))
                        .unwrap_or_else(|| " ?%".into());
                    (name.clone(), l, pct_s, fmt_countdown(reset), draining)
                })
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };

        // model rows
        let mut rows: Vec<DispRow> = Vec::new();
        for m in &st.models {
            if !row_matches(
                m,
                self.pool_filter,
                self.grade_filter.as_deref(),
                &self.search,
            ) {
                continue;
            }
            if self.profile_filter > 0 {
                let want = self.profiles[self.profile_filter - 1].clone();
                if !m.profiles.iter().any(|p| p == &want) {
                    continue;
                }
            }
            let h = st.health.get(&m.id);
            let (quota_pct, reset_in) =
                panel_for(&m.id, &st.quota.provider_map, &st.quota.providers)
                    .map(worst_window)
                    .unwrap_or((None, None));
            let u = st.usage_by_model.get(&m.id);
            let spark = sparkline(
                st.hourly_by_model
                    .get(&m.id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
            );
            rows.push(DispRow {
                grade: m.grade.clone(),
                id: m.id.clone(),
                pools: m.pools.join(","),
                health: match h {
                    None => "ok".into(),
                    Some(h) if h.state == "cooldown" => {
                        format!("cool {}", fmt_countdown(Some(h.secs_left as i64)))
                    }
                    Some(h) => h.state.clone(),
                },
                health_state: h.map(|h| h.state.clone()).unwrap_or_else(|| "ok".into()),
                quota_pct,
                reset_in,
                tok_s: self.tok_ema.get(&m.id).copied().unwrap_or(0.0),
                spark,
                cost_usd: u.map(|u| u.cost_usd).unwrap_or(0.0),
                weight: m.weight,
            });
        }
        sort_rows(&mut rows, self.sort);
        self.rows = rows;
        self.status = Some(st);
    }

    /// Cycle the grade filter through all → each grade → all.
    pub fn cycle_grade(&mut self) {
        self.grade_filter = match &self.grade_filter {
            None => self.grades.first().cloned(),
            Some(g) => {
                let idx = self.grades.iter().position(|x| x == g).unwrap_or(0);
                self.grades.get(idx + 1).cloned()
            }
        };
    }
}

// ─────────────────────────── rendering ───────────────────────────

pub fn ui(f: &mut Frame, mon: &mut Mon) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // LED panel
        Constraint::Length(1), // filter bar
        Constraint::Min(6),    // table
        Constraint::Length(1), // footer / banner
    ])
    .split(f.area());

    draw_header(f, chunks[0], mon);
    draw_led_panel(f, chunks[1], mon);
    draw_filter_bar(f, chunks[2], mon);
    match mon.page {
        Page::Callers => draw_callers(f, chunks[3], mon),
        Page::Engines => draw_engines(f, chunks[3], mon),
        Page::Help => draw_help(f, chunks[3]),
        Page::Models => draw_models(f, chunks[3], mon),
    }
    draw_footer(f, chunks[4], mon);
}

fn draw_header(f: &mut Frame, area: Rect, mon: &Mon) {
    let (ver, up, bind, auth) = match &mon.status {
        Some(s) => (
            s.version.clone(),
            fmt_uptime(s.uptime_secs),
            s.bind.clone(),
            if s.auth_required { "on" } else { "off" },
        ),
        None => ("…".into(), "—".into(), mon.url.clone(), "?"),
    };
    let line = Line::from(vec![
        Span::styled(" arouter", Style::default().bold().fg(Color::Cyan)),
        Span::raw(format!(" v{ver} · {bind} · up {up} · auth {auth}")),
        if mon.active {
            Span::styled("  ● active", Style::default().fg(Color::Green))
        } else {
            Span::styled("  ○ idle", Style::default().fg(Color::DarkGray))
        },
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_led_panel(f: &mut Frame, area: Rect, mon: &Mon) {
    if mon.led_rows.is_empty() {
        let line = Line::from(Span::styled(
            " quota: no provider data",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let mut spans = vec![Span::raw(" ")];
    for (name, led, pct, reset, draining) in &mon.led_rows {
        let color = led.color();
        spans.push(Span::styled(
            format!("{} ", led.glyph()),
            Style::default().fg(color),
        ));
        spans.push(Span::raw(format!("{name} {pct}")));
        if *draining {
            spans.push(Span::styled(
                " ⚡drain",
                Style::default().fg(Color::LightYellow).bold(),
            ));
        }
        if reset != "—" {
            spans.push(Span::styled(
                format!(" ↻{reset}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::raw("  "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_filter_bar(f: &mut Frame, area: Rect, mon: &Mon) {
    let pool = mon.pool_filter.unwrap_or("all");
    let grade = mon.grade_filter.clone().unwrap_or_else(|| "all".into());
    let profile = if mon.profile_filter == 0 {
        "all".to_string()
    } else {
        mon.profiles
            .get(mon.profile_filter - 1)
            .cloned()
            .unwrap_or_else(|| "all".into())
    };
    let search = if mon.search.is_empty() {
        String::new()
    } else {
        format!(" /{}", mon.search)
    };
    let line = Line::from(vec![
        Span::styled(" filters:", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(" pool={pool} grade={grade} profile={profile}")),
        Span::raw(search),
        Span::styled(
            format!("  sort={}", mon.sort.label()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_models(f: &mut Frame, area: Rect, mon: &Mon) {
    let header = [
        "GR", "MODEL", "POOLS", "HEALTH", "QUOTA", "RESET", "TOK/S", "24H", "SPEND", "W",
    ]
    .into_iter()
    .map(|h| Cell::from(h).style(Style::default().fg(Color::DarkGray).bold()));
    let table = Table::new(
        mon.rows.iter().map(|r| {
            let grade_style = match r.grade.as_str() {
                "A+" => Style::default().fg(Color::LightGreen).bold(),
                "A" => Style::default().fg(Color::Green),
                "B" => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::DarkGray),
            };
            let health_style = match r.health_state.as_str() {
                "ok" => Style::default().fg(Color::Green),
                "cooldown" | "open" => Style::default().fg(Color::Red),
                _ => Style::default().fg(Color::Yellow),
            };
            let (bar_s, bar_style) = match r.quota_pct {
                None => ("—".to_string(), Style::default().fg(Color::DarkGray)),
                Some(p) => (
                    format!("{} {:>3.0}%", bar(p, 8), p * 100.0),
                    if p > 0.5 {
                        Style::default().fg(Color::Green)
                    } else if p > 0.15 {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Red)
                    },
                ),
            };
            Row::new(vec![
                Cell::from(r.grade.clone()).style(grade_style),
                Cell::from(r.id.clone()),
                Cell::from(r.pools.clone()).style(Style::default().fg(Color::DarkGray)),
                Cell::from(r.health.clone()).style(health_style),
                Cell::from(bar_s).style(bar_style),
                Cell::from(fmt_countdown(r.reset_in)),
                Cell::from(if r.tok_s > 0.0 {
                    format!("{:.1}", r.tok_s)
                } else {
                    "·".into()
                }),
                Cell::from(r.spark.clone()).style(Style::default().fg(Color::Cyan)),
                Cell::from(fmt_usd(r.cost_usd)).style(Style::default().fg(Color::LightYellow)),
                Cell::from(if r.weight > 0.0 {
                    format!("{}", r.weight)
                } else {
                    "·".into()
                }),
            ])
        }),
        [
            Constraint::Length(3),
            Constraint::Min(26),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(13),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(24),
            Constraint::Length(9),
            Constraint::Length(4),
        ],
    )
    .header(Row::new(header))
    .block(Block::default().borders(Borders::TOP));
    f.render_widget(table, area);
}

fn draw_callers(f: &mut Frame, area: Rect, mon: &Mon) {
    let st = match &mon.status {
        Some(s) => s,
        None => {
            f.render_widget(Paragraph::new(" no data"), area);
            return;
        }
    };
    let mut callers: Vec<_> = st.callers.iter().collect();
    callers.sort_by_key(|(n, _)| n.to_string());
    let header = ["CALLER", "REQ", "OK", "FAIL", "TOKENS", "SPEND", "24H"]
        .into_iter()
        .map(|h| Cell::from(h).style(Style::default().fg(Color::DarkGray).bold()));
    let table = Table::new(
        callers.iter().map(|(name, u)| {
            let spark = sparkline(
                st.hourly_by_caller
                    .get(*name)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]),
            );
            Row::new(vec![
                Cell::from(name.replace('/', " · ")),
                Cell::from(u.requests.to_string()),
                Cell::from(u.ok.to_string()).style(Style::default().fg(Color::Green)),
                Cell::from(u.failed.to_string()).style(if u.failed > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                }),
                Cell::from(format!(
                    "{}↑ {}↓",
                    fmt_tokens(u.tokens_in),
                    fmt_tokens(u.tokens_out)
                )),
                Cell::from(fmt_usd(u.cost_usd)).style(Style::default().fg(Color::LightYellow)),
                Cell::from(spark).style(Style::default().fg(Color::Cyan)),
            ])
        }),
        [
            Constraint::Min(20),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(16),
            Constraint::Length(10),
            Constraint::Length(24),
        ],
    )
    .header(Row::new(header))
    .block(Block::default().borders(Borders::TOP));
    f.render_widget(table, area);
}

fn draw_engines(f: &mut Frame, area: Rect, mon: &Mon) {
    let st = match &mon.status {
        Some(s) => s,
        None => {
            f.render_widget(Paragraph::new(" no data"), area);
            return;
        }
    };
    let le = &st.local_engines;
    if le.engines.is_empty() {
        let p = Paragraph::new(
            " no local engines configured — set [local_engines] poll_secs > 0 and add an engine url",
        )
        .block(Block::default().borders(Borders::TOP));
        f.render_widget(p, area);
        return;
    }
    let now = unix_now();
    let mut title = format!(
        " engines · fetched {}s ago",
        now.saturating_sub(le.fetched_unix)
    );
    if le.stale {
        title.push_str(" · ⚠ stale");
    }
    let mut engines: Vec<_> = le.engines.iter().collect();
    engines.sort_by(|a, b| a.url.cmp(&b.url));
    let header = ["ENGINE", "KIND", "STATE", "MODEL", "VRAM", "KEEP"]
        .into_iter()
        .map(|h| Cell::from(h).style(Style::default().fg(Color::DarkGray).bold()));
    let mut body: Vec<Row> = Vec::new();
    for e in &engines {
        let state = if le.stale {
            ("stale".to_string(), Style::default().fg(Color::DarkGray))
        } else if e.busy {
            ("BUSY".to_string(), Style::default().fg(Color::Red).bold())
        } else {
            ("idle".to_string(), Style::default().fg(Color::Green))
        };
        let host = host_of(&e.url);
        if e.loaded.is_empty() {
            body.push(Row::new(vec![
                Cell::from(host.to_string()),
                Cell::from(e.kind.clone()),
                Cell::from(state.0).style(state.1),
                Cell::from("—"),
                Cell::from("—"),
                Cell::from("—"),
            ]));
            continue;
        }
        let mut loaded: Vec<_> = e.loaded.iter().collect();
        loaded.sort_by(|a, b| a.0.cmp(b.0));
        for (name, lm) in loaded {
            let keep = lm
                .expires_unix
                .map(|exp| fmt_countdown(Some(exp as i64 - now as i64)))
                .unwrap_or_else(|| "—".into());
            body.push(Row::new(vec![
                Cell::from(host.to_string()),
                Cell::from(e.kind.clone()),
                Cell::from(state.0.clone()).style(state.1),
                Cell::from(name.clone()),
                Cell::from(fmt_bytes(lm.size_vram)),
                Cell::from(keep),
            ]));
        }
    }
    let table = Table::new(
        body,
        [
            Constraint::Min(18),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(24),
            Constraint::Length(7),
            Constraint::Length(7),
        ],
    )
    .header(Row::new(header))
    .block(Block::default().borders(Borders::TOP).title(title));
    f.render_widget(table, area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let help = [
        " 1 / 2 / 3   filter pool: quota / local / ondemand (again = all)",
        " g           cycle grade filter",
        " p           cycle profile filter",
        " $           sort by spend · r sort by soonest reset (g = default)",
        " /           search model ids (enter/esc to finish)",
        " Tab         switch models → callers → engines",
        " e           jump to local engines page (E8)",
        " ?           this help · q or Esc quit",
    ];
    let p = Paragraph::new(help.join("\n")).block(Block::default().borders(Borders::TOP));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, mon: &Mon) {
    let line = if let Some(err) = &mon.error {
        let stale = mon
            .last_ok
            .map(|t| t.elapsed().as_secs())
            .map(|s| format!(" (last good data {s}s ago)"))
            .unwrap_or_default();
        Line::from(Span::styled(
            format!(" ⚠ {err}{stale}"),
            Style::default().fg(Color::Red).bold(),
        ))
    } else {
        Line::from(Span::styled(
            " 1/2/3 pool · g grade · p profile · $ sort · r resets · / search · e engines · Tab page · ? help · q quit",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

// ─────────────────────────── event loop ───────────────────────────

/// Run the TUI against `url` (full http://host:port/status.json).
pub async fn run(url: String) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let http = reqwest::Client::new();
    let mut mon = Mon::new(url);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut events = crossterm::event::EventStream::new();
    let res = monitor_loop(&mut terminal, &mut mon, &http, &mut tick, &mut events).await;
    ratatui::restore();
    res
}

async fn monitor_loop(
    terminal: &mut ratatui::DefaultTerminal,
    mon: &mut Mon,
    http: &reqwest::Client,
    tick: &mut tokio::time::Interval,
    events: &mut crossterm::event::EventStream,
) -> anyhow::Result<()> {
    use futures::StreamExt;
    loop {
        terminal.draw(|f| ui(f, mon))?;
        tokio::select! {
            _ = tick.tick() => mon.poll(http).await,
            ev = events.next() => {
                let Some(Ok(ev)) = ev else { break };
                let key = match ev {
                    crossterm::event::Event::Key(k) => k,
                    _ => continue,
                };
                use crossterm::event::{KeyCode, KeyModifiers};
                if mon.search_mode {
                    match key.code {
                        KeyCode::Enter | KeyCode::Esc => mon.search_mode = false,
                        KeyCode::Backspace => { mon.search.pop(); }
                        KeyCode::Char(c) => mon.search.push(c),
                        _ => {}
                    }
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q' | 'c'), ref m) if m.contains(KeyModifiers::CONTROL) => break,
                    (KeyCode::Char('q') | KeyCode::Esc, _) => break,
                    (KeyCode::Tab, _) => mon.page = match mon.page {
                        Page::Models => Page::Callers,
                        Page::Callers => Page::Engines,
                        Page::Engines | Page::Help => Page::Models,
                    },
                    (KeyCode::Char('e'), _) => {
                        mon.page = if mon.page == Page::Engines {
                            Page::Models
                        } else {
                            Page::Engines
                        }
                    }
                    (KeyCode::Char('1'), _) => mon.pool_filter = toggle_pool(mon.pool_filter, "quota"),
                    (KeyCode::Char('2'), _) => mon.pool_filter = toggle_pool(mon.pool_filter, "local"),
                    (KeyCode::Char('3'), _) => mon.pool_filter = toggle_pool(mon.pool_filter, "ondemand"),
                    (KeyCode::Char('g'), _) => mon.cycle_grade(),
                    (KeyCode::Char('p'), _) => mon.profile_filter = (mon.profile_filter + 1) % (mon.profiles.len() + 1),
                    (KeyCode::Char('$'), _) => mon.sort = SortMode::Spend,
                    (KeyCode::Char('r'), _) => mon.sort = SortMode::ResetsSoon,
                    (KeyCode::Char('/'), _) => { mon.search_mode = true; }
                    (KeyCode::Char('?'), _) => mon.page = if mon.page == Page::Help { Page::Models } else { Page::Help },
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn toggle_pool(cur: Option<&'static str>, want: &'static str) -> Option<&'static str> {
    if cur == Some(want) {
        None
    } else {
        Some(want)
    }
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_levels_and_zeroes() {
        let s = sparkline(&[0, 1, 5, 10]);
        let cs: Vec<char> = s.chars().collect();
        assert_eq!(cs.len(), 4);
        assert_eq!(cs[0], ' ', "zero renders as space");
        assert_eq!(cs[3], '█', "max is full block");
        assert!(cs[1] != cs[3] && cs[2] != cs[3]);
        assert_eq!(sparkline(&[0; 24]), " ".repeat(24), "all-zero stays blank");
    }

    #[test]
    fn led_classification() {
        assert_eq!(led(Some(0.98), false, false), Led::Green);
        assert_eq!(
            led(Some(0.95), true, false),
            Led::Amber,
            "notes force amber"
        );
        assert_eq!(led(Some(0.7), false, false), Led::Amber);
        assert_eq!(led(Some(0.0), false, false), Led::Red);
        assert_eq!(led(None, false, false), Led::Gray);
        assert_eq!(
            led(Some(0.98), false, true),
            Led::Amber,
            "fetch error forces amber"
        );
    }

    #[test]
    fn countdown_formats() {
        assert_eq!(fmt_countdown(None), "—");
        assert_eq!(fmt_countdown(Some(0)), "now");
        assert_eq!(fmt_countdown(Some(-5)), "now");
        assert_eq!(fmt_countdown(Some(45)), "45s");
        assert_eq!(fmt_countdown(Some(245)), "4m05s");
        assert_eq!(fmt_countdown(Some(4980)), "1h23m");
    }

    #[test]
    fn token_and_usd_formats() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1500), "1.5k");
        assert_eq!(fmt_tokens(2_400_000), "2.4M");
        assert_eq!(fmt_usd(0.0), "—");
        assert_eq!(fmt_usd(0.000123), "0.000123");
        assert_eq!(fmt_usd(1.5), "1.50");
    }

    #[test]
    fn local_engines_status_parses() {
        // exact shape proxy.rs emits in /status.json
        let v = serde_json::json!({
            "local_engines": {
                "fetched_unix": 1791752400,
                "stale": false,
                "engines": [
                    {
                        "url": "http://127.0.0.1:11434",
                        "kind": "ollama",
                        "busy": false,
                        "loaded": {
                            "gemma3:12b": {"size_vram": 8103792640u64, "expires_unix": 1791752700}
                        }
                    },
                    {
                        "url": "http://127.0.0.1:8081",
                        "kind": "llamacpp",
                        "busy": true,
                        "loaded": {}
                    }
                ]
            }
        });
        let le: LocalEnginesStatus = serde_json::from_value(v["local_engines"].clone()).unwrap();
        assert_eq!(le.engines.len(), 2);
        let ollama = &le.engines[0];
        assert_eq!(host_of(&ollama.url), "127.0.0.1:11434");
        let gemma = ollama.loaded.get("gemma3:12b").unwrap();
        assert_eq!(gemma.size_vram, 8103792640);
        assert_eq!(gemma.expires_unix, Some(1791752700));
        assert!(le.engines[1].busy);
        assert_eq!(le.engines[1].loaded.len(), 0);

        // missing section entirely (poll_secs = 0) → defaults
        let none: LocalEnginesStatus = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(none.engines.is_empty());
    }

    #[test]
    fn byte_formats() {
        assert_eq!(fmt_bytes(0), "—");
        assert_eq!(fmt_bytes(512 * 1024 * 1024), "512M");
        assert_eq!(fmt_bytes(8103792640), "7.5G");
        assert_eq!(fmt_bytes(4096), "4096");
    }

    #[test]
    fn host_of_variants() {
        assert_eq!(host_of("http://127.0.0.1:11434"), "127.0.0.1:11434");
        assert_eq!(host_of("https://gpu.box:8081/base"), "gpu.box:8081");
        assert_eq!(host_of("127.0.0.1:9"), "127.0.0.1:9");
    }

    #[test]
    fn ema_math() {
        assert!((ema(0.0, 100.0) - 30.0).abs() < 1e-9);
        assert!((ema(100.0, 100.0) - 100.0).abs() < 1e-9);
        let settled = (0..50).fold(0.0, |a, _| ema(a, 10.0));
        assert!((settled - 10.0).abs() < 1e-6, "converges to the sample");
    }

    #[test]
    fn filters_match() {
        let m = ModelRow {
            id: "zai/glm-5.2".into(),
            grade: "A".into(),
            pools: vec!["quota".into()],
            ..Default::default()
        };
        assert!(row_matches(&m, None, None, ""));
        assert!(row_matches(&m, Some("quota"), None, ""));
        assert!(!row_matches(&m, Some("local"), None, ""));
        assert!(row_matches(&m, None, Some("A"), ""));
        assert!(!row_matches(&m, None, Some("A+"), ""));
        assert!(row_matches(&m, None, None, "glm"));
        assert!(!row_matches(&m, None, None, "gpt"));
    }

    #[test]
    fn sorting_modes() {
        let mk = |grade: &str, cost: f64, reset: Option<i64>, id: &str| DispRow {
            grade: grade.into(),
            cost_usd: cost,
            reset_in: reset,
            id: id.into(),
            ..Default::default()
        };
        let mut rows = vec![
            mk("B", 9.0, None, "b/1"),
            mk("A", 0.5, Some(3600), "a/2"),
            mk("A+", 0.1, Some(60), "c/3"),
        ];
        sort_rows(&mut rows, SortMode::Grade);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["c/3", "a/2", "b/1"],
            "grade asc is primary"
        );
        sort_rows(&mut rows, SortMode::Spend);
        assert_eq!(rows[0].id, "b/1", "spend desc");
        sort_rows(&mut rows, SortMode::ResetsSoon);
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["c/3", "a/2", "b/1"],
            "soonest reset first, no-reset last"
        );
    }

    #[test]
    fn worst_window_and_panel_lookup() {
        let mut providers = HashMap::new();
        providers.insert(
            "codex".to_string(),
            Provider {
                windows: vec![
                    QuotaWindow {
                        label: "5h".into(),
                        pct: 90.0,
                        reset_in_secs: Some(7000),
                    },
                    QuotaWindow {
                        label: "wk".into(),
                        pct: 42.0,
                        reset_in_secs: Some(200_000),
                    },
                ],
                ..Default::default()
            },
        );
        let mut map = HashMap::new();
        map.insert("stub".to_string(), "codex".to_string());

        let p = panel_for("stub/any", &map, &providers).expect("via provider_map");
        let (pct, reset) = worst_window(p);
        assert!((pct.unwrap() - 0.42).abs() < 1e-9, "worst window wins");
        assert_eq!(reset, Some(7000), "earliest positive reset");

        let direct = panel_for("codex/x", &map, &providers).expect("prefix match");
        assert_eq!(direct.windows.len(), 2);
        assert!(panel_for("other/x", &map, &providers).is_none());
    }

    #[test]
    fn bar_renders_fills() {
        assert_eq!(bar(1.0, 4), "▓▓▓▓");
        assert_eq!(bar(0.0, 4), "░░░░");
        assert_eq!(bar(0.5, 4), "▓▓░░");
        assert_eq!(bar(2.0, 3), "▓▓▓", "overfull clamps");
    }

    #[test]
    fn status_json_parses() {
        let raw = serde_json::json!({
            "version": "0.1.0",
            "models": [{"id": "a/b", "grade": "A", "pools": ["quota"], "weight": 0.5,
                        "provenance": "catalog", "extra_future_field": true}],
            "quota": {"stale": false, "providers": {}, "provider_map": {"a": "z"}},
            "usage_by_model": {"a/b": {"requests": 3, "tokens_in": 10, "tokens_out": 20, "cost_usd": 0.5}},
            "requests_total": 3,
            "health": {"a/b": {"state": "cooldown", "secs_left": 42, "fail_ratio_1h": 0.5}}
        });
        let st: Status = serde_json::from_value(raw).expect("tolerant parse");
        assert_eq!(st.models.len(), 1);
        assert_eq!(st.usage_by_model["a/b"].tokens_out, 20);
        assert_eq!(st.health["a/b"].state, "cooldown");
    }
}
