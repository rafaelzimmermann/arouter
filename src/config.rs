//! Configuration: TOML schema, defaults, validation, and the key store
//! (resolves `env:NAME` auth specs against the process env plus
//! `~/.config/arouter/.env` and `~/.config/aitop/.env` — single key source
//! shared with aitop).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const POOLS: [&str; 3] = ["quota", "local", "ondemand"];
pub const RESERVED_PROFILES: [&str; 3] = ["default", "local", "ondemand"];

// ────────────────────────────── schema ──────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub grades: GradesConfig,
    pub routing: RoutingConfig,
    /// Upstream deadlines (review F5): a connected but stalled upstream
    /// must never block failover — headers, body reads, and SSE chunk
    /// gaps are all bounded, and the candidate list is capped.
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    #[serde(default)]
    pub quota_watcher: QuotaWatcherConfig,
    /// E3b native intake: `[quota]` — the router asks its upstreams
    /// directly. `[quota_watcher]` (external command) is still accepted
    /// as a legacy alias; `[quota]` wins where both are set.
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub local_engines: LocalEnginesConfig,
    pub pool: BTreeMap<String, PoolConfig>,
    pub profiles: BTreeMap<String, ProfileConfig>,
    pub models: Vec<ModelConfig>,
    pub auth: AuthConfig,
    pub callers: Vec<CallerConfig>,
}

/// Upstream deadline policy (review F5). A connected upstream that
/// stalls — before headers, mid-body, or between SSE chunks — is cut
/// so the next candidate can be tried. Also bounds how many
/// candidates one request may burn through.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct TimeoutsConfig {
    /// TCP/TLS connect budget per attempt.
    pub connect_secs: u64,
    /// Deadline for response HEADERS after the request is written.
    /// A stalled-here upstream fails over to the next candidate.
    pub headers_secs: u64,
    /// Maximum gap between body reads (SSE chunk gaps included). Also
    /// bounds buffered body reads. Matches the documented "idle 120s".
    pub idle_secs: u64,
    /// Candidates tried per request (grade order preserved). 0 is
    /// rejected; 1 disables failover.
    pub max_attempts: usize,
}

impl Default for TimeoutsConfig {
    /// The documented defaults: idle 120s, headers 60s, top-3 attempts.
    fn default() -> Self {
        Self {
            connect_secs: 5,
            headers_secs: 60,
            idle_secs: 120,
            max_attempts: 3,
        }
    }
}

impl TimeoutsConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.connect_secs == 0 {
            return Err("timeouts.connect_secs must be >= 1".into());
        }
        if self.headers_secs == 0 {
            return Err("timeouts.headers_secs must be >= 1".into());
        }
        if self.idle_secs == 0 {
            return Err("timeouts.idle_secs must be >= 1".into());
        }
        if self.max_attempts == 0 || self.max_attempts > 16 {
            return Err("timeouts.max_attempts must be 1..=16".into());
        }
        Ok(())
    }

    /// Sane non-stalling defaults for background JSON polling
    /// (quota engine, discovery): quick connect, short reads.
    pub fn poll() -> Self {
        Self {
            connect_secs: 5,
            headers_secs: 15,
            idle_secs: 15,
            max_attempts: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub state_dir: String,
    /// Extra env file to load keys from (default files are always loaded).
    pub env_file: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:3929".into(),
            state_dir: expand_tilde("~/.local/state/arouter"),
            env_file: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GradesConfig {
    pub ladder: Vec<String>,
}

impl GradesConfig {
    pub fn ladder(&self) -> crate::grades::Ladder {
        if self.ladder.is_empty() {
            crate::grades::Ladder::default()
        } else {
            crate::grades::Ladder::new(self.ladder.clone())
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    pub fallback: FallbackConfig,
    /// Warm-cache stickiness for the local pool (E8): consecutive requests
    /// from one caller for one grade stick to the model that just served
    /// them so the engine's prompt/KV cache stays hot. 0 disables.
    pub sticky_local_secs: u64,
    /// Seconds between upstream model discovery polls (0 = off; SIGHUP
    /// always triggers an immediate re-discovery + reload).
    pub discovery_interval_secs: u64,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            fallback: FallbackConfig::default(),
            // warm-cache stickiness is local-pool-only and grade-preserving,
            // so it is safe to enable out of the box
            sticky_local_secs: 600,
            discovery_interval_secs: 0,
        }
    }
}

impl RoutingConfig {
    pub fn discovery_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.discovery_interval_secs)
    }
}

/// Local-engine awareness (E8): poll ollama/llama.cpp engines so routing
/// can prefer resident models and avoid busy ones.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LocalEnginesConfig {
    /// Seconds between engine polls (0 = off; arouter then applies no
    /// residency boost and routing behaves as before).
    pub poll_secs: u64,
    pub engines: Vec<LocalEngineConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct LocalEngineConfig {
    /// Engine root, e.g. `http://127.0.0.1:11434` (no `/v1`).
    pub url: String,
    /// `ollama` | `llamacpp` | `auto` (default: auto — probe `/api/ps`,
    /// fall back to `/slots`).
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QuotaWatcherConfig {
    /// Shell-free command line (whitespace split) to run for a snapshot.
    /// Default: `aitop --json`.
    pub command: Option<String>,
    /// Seconds between snapshots (0 disables the watcher; arouter then
    /// routes on health data only and treats quota as unknown).
    pub interval_secs: u64,
    /// Registry provider prefix → aitop panel name (e.g. `zai = "z.ai"`).
    /// Unmapped providers fall back to exact + fuzzy matching.
    pub provider_map: BTreeMap<String, String>,
}

impl Default for QuotaWatcherConfig {
    fn default() -> Self {
        Self {
            command: None,
            interval_secs: 60,
            provider_map: BTreeMap::new(),
        }
    }
}

impl QuotaWatcherConfig {
    pub fn command_or_default(&self) -> String {
        self.command
            .clone()
            .unwrap_or_else(|| "aitop --json".into())
    }
}

/// E3b: per-provider manual quota windows (for providers with no quota
/// API at all — pinned by the owner, edited by the web UI later).
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct QuotaManualWindow {
    /// Registry provider prefix (e.g. `zai`).
    pub provider: String,
    /// Window label shown in monitor/status (e.g. `weekly`).
    pub label: String,
    /// Percent used.
    pub pct: f64,
    /// Seconds until reset (absent/0 = never resets).
    pub resets_in_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct QuotaConfig {
    /// Seconds between native polls (0 disables the engine; quota then
    /// comes only from harvested headers and manual windows).
    pub poll_secs: u64,
    /// Provider prefix → source spec: `headers` | `off` |
    /// `native:openrouter` | `native:zai` | `manual`.
    /// Unlisted providers: openrouter.ai → native:openrouter (documented),
    /// everything else → headers.
    pub sources: BTreeMap<String, String>,
    /// Legacy escape hatch: run an external command for a whole snapshot
    /// (aitop-shaped JSON). Mutually exclusive with native sources per
    /// provider — explicit sources win.
    pub command: Option<String>,
    /// Registry provider prefix → panel name (as before, now per [quota]).
    pub provider_map: BTreeMap<String, String>,
    /// Manually pinned windows.
    pub manual: Vec<QuotaManualWindow>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FallbackConfig {
    /// Pools this pool may fall back into when fully exhausted (opt-in).
    /// Absent/None = none. A profile overlay setting `Some([])` explicitly
    /// DISABLES an inherited base list (empty ≠ unspecified — review F7).
    pub cross_pool: Option<Vec<String>>,
    /// Which grades may serve each requested grade (missing key = default).
    /// Never upward: entries above the requested grade are rejected in
    /// validation and filtered at selection (review F10).
    pub allow: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PoolConfig {
    /// `cheapest` | `fastest` | `manual` (docs/routing.md §4)
    pub preference: String,
    pub budget: Option<BudgetConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(default)]
pub struct BudgetConfig {
    pub daily_usd: f64,
    pub monthly_usd: f64,
    pub pace_hourly_usd: f64,
    pub per_request_usd: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProfileConfig {
    pub description: String,
    pub preference: Option<String>,
    pub fallback: Option<FallbackConfig>,
    pub budget: Option<BudgetConfig>,
    pub models: Option<VisibilityConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct VisibilityConfig {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Registry id: `provider/model` (the model part defaults the upstream name).
    pub id: String,
    /// Upstream model name sent to the provider; defaults to the part after `/`.
    pub name: Option<String>,
    pub grade: String,
    pub pool: Vec<String>,
    pub context: Option<u64>,
    pub base_url: String,
    /// Auth spec: `env:NAME`, `key:LITERAL`, `none`, or bare env name.
    pub auth: Option<String>,
    pub weight: f64,
    /// $/Mtok input/output (on-demand pool).
    pub cost_in: f64,
    pub cost_out: f64,
    /// Empty = visible in all profiles; otherwise only these profiles.
    pub profiles: Vec<String>,
    /// Set by auto-grading for discovered models (`catalog` /
    /// `heuristic` / `auto`); manual config entries report `manual`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provenance: String,
    /// Rationale string for heuristic/auto grades (auditable).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
    /// Wire-compat role rewrite applied to outgoing `messages` (e.g.
    /// `{ developer = "system" }` for upstreams that predate OpenAI's
    /// `developer` role). Keys/values are chat-completion role names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_map: Option<std::collections::BTreeMap<String, String>>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: None,
            grade: String::new(),
            pool: vec![],
            context: None,
            base_url: String::new(),
            auth: None,
            weight: 1.0,
            cost_in: 0.0,
            cost_out: 0.0,
            profiles: vec![],
            provenance: String::new(),
            rationale: String::new(),
            role_map: None,
        }
    }
}

impl ModelConfig {
    pub fn upstream_name(&self) -> &str {
        match &self.name {
            Some(n) => n,
            None => self.id.rsplit('/').next().unwrap_or(&self.id),
        }
    }
}

// ─────────────────────────── callers (multi-user) ───────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    /// true → requests must present a known caller key (401 otherwise) and
    /// `X-Arouter-User`/`-Project` headers are ignored (spoofable without auth).
    pub require: bool,
}

/// A named caller of arouter (developer / bot / pi entry). Distinct API keys
/// let multiple users share one daemon with per-caller metrics; `project`
/// tags usage for per-project tracking.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CallerConfig {
    pub name: String,
    pub project: Option<String>,
    /// Auth spec like models: `env:NAME` / `key:LITERAL` / `none`.
    pub key: Option<String>,
    /// Per-caller daily cap in USD for the on-demand pool (E4): stacks
    /// below pool/profile caps — the tightest wins.
    pub budget_usd_day: Option<f64>,
}

pub const CALLER_ANONYMOUS: &str = "anonymous";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    pub name: String,
    pub project: Option<String>,
}

impl CallerIdentity {
    pub fn anonymous() -> Self {
        Self {
            name: CALLER_ANONYMOUS.to_string(),
            project: None,
        }
    }

    pub fn is_anonymous(&self) -> bool {
        self.name == CALLER_ANONYMOUS
    }

    /// `name` or `name/project` — key for stats maps and log labels.
    pub fn stat_key(&self) -> String {
        match &self.project {
            Some(p) => format!("{}/{}", self.name, p),
            None => self.name.clone(),
        }
    }
}

/// Resolved caller table: api key → identity. Built once at startup from
/// config + key store; lookups are lock-free reads of immutable data.
#[derive(Debug, Default)]
pub struct CallerMap {
    by_key: BTreeMap<String, CallerIdentity>,
    require: bool,
}

impl CallerMap {
    pub fn from_config(cfg: &Config, keys: &KeyStore) -> Self {
        let mut by_key = BTreeMap::new();
        for c in &cfg.callers {
            match keys.resolve(&c.key) {
                Some(k) => {
                    by_key.insert(
                        k,
                        CallerIdentity {
                            name: c.name.clone(),
                            project: c.project.clone(),
                        },
                    );
                }
                None => tracing::warn!(
                    caller = c.name,
                    "key spec unresolved; caller not reachable via auth"
                ),
            }
        }
        Self {
            by_key,
            require: cfg.auth.require,
        }
    }

    /// Resolve the caller of a request.
    /// `bearer` = token from `Authorization: Bearer …` or `X-Api-Key`;
    /// `user_hdr`/`project_hdr` = `X-Arouter-User`/`X-Arouter-Project` values
    /// (open mode only). Err(message) → 401.
    pub fn resolve(
        &self,
        bearer: Option<&str>,
        user_hdr: Option<&str>,
        project_hdr: Option<&str>,
    ) -> Result<CallerIdentity, String> {
        let bearer = bearer.map(str::trim).filter(|t| !t.is_empty());
        if let Some(tok) = bearer {
            if let Some(id) = self.by_key.get(tok) {
                return Ok(id.clone());
            }
            if self.require {
                return Err("unknown api key".to_string());
            }
            // open mode: unknown keys degrade to the header/anonymous path
            tracing::warn!(
                "unknown api key presented; continuing as anonymous (set [auth] require to reject)"
            );
        }
        if self.require {
            return Err("missing api key ([auth] require = true)".to_string());
        }
        if let Some(u) = user_hdr.map(str::trim).filter(|t| !t.is_empty()) {
            return Ok(CallerIdentity {
                name: u.to_string(),
                project: project_hdr
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            });
        }
        Ok(CallerIdentity::anonymous())
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

// ─────────────────────────── load & validate ───────────────────────────

impl Config {
    /// Load from an explicit path (must exist) or the default location
    /// (`$AROUTER_CONFIG`, else `~/.config/arouter/config.toml`); missing
    /// default config falls back to built-in defaults.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Self> {
        let path = match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => default_config_path(),
        };
        match path {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(&p)
                    .map_err(|e| anyhow::anyhow!("read {}: {e}", p.display()))?;
                let cfg: Config = toml::from_str(&raw)
                    .map_err(|e| anyhow::anyhow!("parse {}: {e}", p.display()))?;
                let cfg = cfg.merge_legacy_quota();
                cfg.validate().map_err(anyhow::Error::msg)?;
                Ok(cfg)
            }
            Some(p) if explicit.is_some() => {
                anyhow::bail!("config not found: {}", p.display())
            }
            _ => {
                tracing::warn!(
                    "no config at default path; using built-in defaults (see config.example.toml)"
                );
                let cfg = Config::default();
                cfg.validate().map_err(anyhow::Error::msg)?;
                Ok(cfg)
            }
        }
    }

    /// E3b: `[quota_watcher]` → `[quota]` legacy merge. Fields already
    /// set in `[quota]` win; the alias only fills gaps so existing
    /// configs (and the integration rig) keep working unchanged.
    pub fn merge_legacy_quota(mut self) -> Self {
        let w = self.quota_watcher.clone();
        if self.quota.poll_secs == 0 {
            self.quota.poll_secs = w.interval_secs;
        }
        if self.quota.command.is_none() {
            self.quota.command = w.command.clone();
        }
        for (k, v) in w.provider_map {
            self.quota.provider_map.entry(k).or_insert(v);
        }
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        // E3b: source specs must resolve
        for (prefix, spec) in &self.quota.sources {
            let known = crate::quotaeng::parse_spec(spec).is_some()
                || spec == "command"
                || spec.starts_with("command:");
            if !known {
                return Err(format!(
                    "quota.sources.{prefix}: unknown source {spec:?} \
                     (headers | off | native:openrouter | native:zai | manual | command:<cmd>)"
                ));
            }
        }
        let ladder = self.grades.ladder();
        if ladder.grades.len() < 2 {
            return Err("grades.ladder needs at least two grades".into());
        }
        if ladder.grades.len()
            != ladder
                .grades
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        {
            return Err("grades.ladder has duplicate grades".into());
        }
        for (name, _p) in &self.profiles {
            if RESERVED_PROFILES.contains(&name.as_str()) {
                return Err(format!("profile name {name:?} is reserved"));
            }
            if let Some(v) = &_p.models {
                if !v.include.is_empty() && !v.exclude.is_empty() {
                    return Err(format!(
                        "profile {name}: include and exclude are mutually exclusive"
                    ));
                }
            }
        }
        let mut seen_callers = std::collections::HashSet::new();
        if self.auth.require && self.callers.is_empty() {
            return Err("auth.require = true needs at least one [[callers]] entry".into());
        }
        for c in &self.callers {
            if c.name.is_empty() {
                return Err("caller name must not be empty".into());
            }
            if c.name == CALLER_ANONYMOUS {
                return Err(format!("caller name {:?} is reserved", c.name));
            }
            if !seen_callers.insert(c.name.clone()) {
                return Err(format!("duplicate caller {:?}", c.name));
            }
        }
        for (name, allow) in &self.routing.fallback.allow {
            let Some(ceiling) = ladder.rank(name) else {
                return Err(format!("routing.fallback.allow: unknown grade {name:?}"));
            };
            for t in allow {
                let Some(r) = ladder.rank(t) else {
                    return Err(format!(
                        "routing.fallback.allow[{name:?}]: unknown grade {t:?}"
                    ));
                };
                if r < ceiling {
                    return Err(format!(
                        "routing.fallback.allow[{name:?}]: {t:?} is above the requested grade — fallback never routes upward"
                    ));
                }
            }
        }
        for (name, pc) in &self.pool {
            if !POOLS.contains(&name.as_str()) {
                return Err(format!("unknown pool {name:?} (valid: {POOLS:?})"));
            }
            if !pc.preference.is_empty()
                && !["cheapest", "fastest", "manual"].contains(&pc.preference.as_str())
            {
                return Err(format!(
                    "pool.{name}.preference: unknown {:?}",
                    pc.preference
                ));
            }
        }
        for m in &self.models {
            if m.id.is_empty() {
                return Err("model id must not be empty".into());
            }
            if m.base_url.is_empty() {
                return Err(format!("model {}: base_url is required", m.id));
            }
            if ladder.rank(&m.grade).is_none() {
                return Err(format!("model {}: unknown grade {:?}", m.id, m.grade));
            }
            if m.pool.is_empty() {
                return Err(format!("model {}: at least one pool is required", m.id));
            }
            for p in &m.pool {
                if !POOLS.contains(&p.as_str()) {
                    return Err(format!(
                        "model {}: unknown pool {p:?} (valid: {POOLS:?})",
                        m.id
                    ));
                }
            }
            for pr in &m.profiles {
                if RESERVED_PROFILES.contains(&pr.as_str()) {
                    return Err(format!("model {}: {pr:?} is reserved", m.id));
                }
                if !self.profiles.contains_key(pr) {
                    return Err(format!("model {}: unknown profile {pr:?}", m.id));
                }
            }
        }
        self.timeouts.validate()?;
        Ok(())
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AROUTER_CONFIG") {
        return Some(PathBuf::from(p));
    }
    home_dir().map(|h| h.join(".config/arouter/config.toml"))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
}

pub fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

// ────────────────────────────── key store ──────────────────────────────

/// Resolves auth specs against the process environment plus env files.
#[derive(Debug, Default, Clone)]
pub struct KeyStore {
    vars: BTreeMap<String, String>,
}

impl KeyStore {
    /// Load `~/.config/arouter/.env` then `~/.config/aitop/.env`
    /// (aitop wins only for keys arouter's file doesn't define), plus an
    /// optional extra path from config.
    pub fn load_default(extra: Option<&str>) -> Self {
        let mut vars = BTreeMap::new();
        let mut paths: Vec<PathBuf> = vec![];
        if let Some(e) = extra {
            paths.push(PathBuf::from(crate::config::expand_tilde(e)));
        }
        if let Some(h) = home_dir() {
            paths.push(h.join(".config/arouter/.env"));
            paths.push(h.join(".config/aitop/.env"));
        }
        for p in paths {
            if let Ok(raw) = std::fs::read_to_string(&p) {
                for (k, v) in parse_env_file(&raw) {
                    vars.entry(k).or_insert(v); // earlier files win
                }
            }
        }
        Self { vars }
    }

    /// Resolve an auth spec: `env:NAME` / `key:LITERAL` / `none` / bare env name.
    pub fn resolve(&self, spec: &Option<String>) -> Option<String> {
        let spec = spec.as_deref()?.trim().to_string();
        if spec.is_empty() || spec == "none" {
            return None;
        }
        if let Some(name) = spec.strip_prefix("key:") {
            return Some(name.to_string());
        }
        let name = spec.strip_prefix("env:").unwrap_or(&spec);
        std::env::var(name)
            .ok()
            .or_else(|| self.vars.get(name).cloned())
    }
}

/// Parse KEY=VALUE lines; ignores blanks, `#` comments; strips quotes.
pub fn parse_env_file(raw: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim().to_string();
        let mut v = v.trim().to_string();
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v = v[1..v.len() - 1].to_string();
        }
        if !k.is_empty() {
            out.push((k, v));
        }
    }
    out
}

// ────────────────────── grades.auto.toml (generated; E2) ──────────────────────

/// Persisted auto-grades written by `arouter models grade --write`
/// (docs/grading.md). Sits between manual config and the built-in
/// catalog in the precedence chain; safe to delete — regenerated.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AutoGrades {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<AutoModelEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoModelEntry {
    pub id: String,
    pub grade: String,
    #[serde(default)]
    pub pool: Vec<String>,
    #[serde(default)]
    pub provenance: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rationale: String,
}

impl AutoGrades {
    /// Path of `grades.auto.toml` next to the config file.
    pub fn path_for(explicit: Option<&Path>) -> Option<PathBuf> {
        let base = match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => default_config_path(),
        }?;
        base.parent().map(|d| d.join("grades.auto.toml"))
    }

    /// Best-effort load; a missing or malformed file is not an error
    /// (the file is regenerated output, not user input).
    pub fn load(explicit: Option<&Path>) -> Self {
        let Some(p) = Self::path_for(explicit) else {
            return Self::default();
        };
        match std::fs::read_to_string(&p) {
            Ok(raw) => match toml::from_str(&raw) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("ignoring {}: {e}", p.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn write(&self, explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
        let p = Self::path_for(explicit)
            .ok_or_else(|| anyhow::anyhow!("no config directory for grades.auto.toml"))?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(p)
    }

    pub fn grade_for(&self, id: &str) -> Option<&AutoModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn timeouts_defaults_match_the_docs_and_validate() {
        // review F5: documented defaults idle 120s / top-3, all bounded
        let t = TimeoutsConfig::default();
        assert_eq!(
            (t.connect_secs, t.headers_secs, t.idle_secs, t.max_attempts),
            (5, 60, 120, 3)
        );
        assert!(t.validate().is_ok());
        for bad in [
            TimeoutsConfig {
                connect_secs: 0,
                ..t.clone()
            },
            TimeoutsConfig {
                headers_secs: 0,
                ..t.clone()
            },
            TimeoutsConfig {
                idle_secs: 0,
                ..t.clone()
            },
            TimeoutsConfig {
                max_attempts: 0,
                ..t.clone()
            },
            TimeoutsConfig {
                max_attempts: 17,
                ..t.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        // missing [timeouts] section → defaults, valid
        let mut c = Config::default();
        c.models = vec![model("x/m", "B", &["quota"])];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn upward_fallback_allow_is_rejected() {
        // review F10: fail loud at load, not silently at routing time
        let mut c = Config::default();
        c.models = vec![model("x/m", "B", &["quota"])];
        c.routing
            .fallback
            .allow
            .insert("B".into(), vec!["B".into(), "A+".into()]);
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("never routes upward"), "{err}");
        // same-grade and downward stay legal
        c.routing
            .fallback
            .allow
            .insert("B".into(), vec!["B".into(), "C".into()]);
        c.validate().unwrap();
    }

    fn model(id: &str, grade: &str, pool: &[&str]) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            grade: grade.into(),
            pool: pool.iter().map(|s| s.to_string()).collect(),
            base_url: "http://upstream.test/v1".into(),
            ..ModelConfig::default()
        }
    }

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.server.bind, "127.0.0.1:3929");
        assert_eq!(cfg.grades.ladder().token(0), "A+");
    }

    #[test]
    fn toml_roundtrip_minimal() {
        let raw = r#"
[[models]]
id = "zai/glm-5.3"
grade = "A+"
pool = ["quota"]
base_url = "https://api.z.ai/api/coding/paas/v4"
auth = "env:ZAI_API_KEY"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].upstream_name(), "glm-5.3");
        assert_eq!(cfg.models[0].weight, 1.0); // default
    }

    #[test]
    fn upstream_name_override() {
        let mut m = model("ollama/gemma", "B", &["local"]);
        assert_eq!(m.upstream_name(), "gemma");
        m.name = Some("gemma4:26b".into());
        assert_eq!(m.upstream_name(), "gemma4:26b");
    }

    #[test]
    fn role_map_parses_and_defaults_to_absent() {
        let raw = r#"
[[models]]
id = "deepseek/deepseek-v4-pro"
grade = "A+"
pool = ["ondemand"]
base_url = "https://api.deepseek.com/v1"
role_map = { developer = "system" }
[[models]]
id = "zai/glm-5.3"
grade = "A+"
pool = ["quota"]
base_url = "https://x"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(
            cfg.models[0].role_map.as_ref().unwrap()["developer"],
            "system"
        );
        assert!(cfg.models[1].role_map.is_none());

        // …and flows into the runtime registry model
        let keys = KeyStore::default();
        let reg = crate::registry::Registry::from_config(&cfg, &keys).unwrap();
        assert_eq!(
            reg.models[0].role_map.get("developer").map(String::as_str),
            Some("system")
        );
        assert!(reg.models[1].role_map.is_empty());
    }

    #[test]
    fn validation_catches_errors() {
        let mut cfg = Config::default();
        cfg.models.push(model("x/bad", "F", &["quota"]));
        assert!(cfg.validate().unwrap_err().contains("unknown grade"));

        let mut cfg = Config::default();
        cfg.models.push(model("x/badpool", "A", &["cloud"]));
        assert!(cfg.validate().unwrap_err().contains("unknown pool"));

        let mut cfg = Config::default();
        cfg.profiles
            .insert("local".into(), ProfileConfig::default());
        assert!(cfg.validate().unwrap_err().contains("reserved"));

        let mut cfg = Config::default();
        cfg.profiles.insert(
            "night".into(),
            ProfileConfig {
                models: Some(VisibilityConfig {
                    include: vec!["*".into()],
                    exclude: vec!["*opus*".into()],
                }),
                ..Default::default()
            },
        );
        assert!(cfg.validate().unwrap_err().contains("mutually exclusive"));
    }

    #[test]
    fn env_file_parsing() {
        let raw = "# comment\n\nZAI_API_KEY=abc\nOPENROUTER_API_KEY=\"sk-or-1\"\nEMPTY=\nBROKEN\nQUOTED='x=y'\n";
        let vars = parse_env_file(raw);
        let m: BTreeMap<_, _> = vars.into_iter().collect();
        assert_eq!(m.get("ZAI_API_KEY").unwrap(), "abc");
        assert_eq!(m.get("OPENROUTER_API_KEY").unwrap(), "sk-or-1");
        assert_eq!(m.get("QUOTED").unwrap(), "x=y");
        assert!(m.contains_key("EMPTY"));
        assert!(!m.contains_key("BROKEN"));
    }

    #[test]
    fn keystore_resolution() {
        let ks = KeyStore::default();
        // key: literal
        assert_eq!(
            ks.resolve(&Some("key:literal-token".into())).as_deref(),
            Some("literal-token")
        );
        // none / empty
        assert_eq!(ks.resolve(&Some("none".into())), None);
        assert_eq!(ks.resolve(&Some("".into())), None);
        assert_eq!(ks.resolve(&None), None);
    }

    #[test]
    fn keystore_env_file_lookup() {
        let mut vars = BTreeMap::new();
        vars.insert("AROUTER_TEST_KEY".to_string(), "from-file".to_string());
        let ks = KeyStore { vars };
        assert_eq!(
            ks.resolve(&Some("env:AROUTER_TEST_KEY".into())).as_deref(),
            Some("from-file")
        );
        // real env wins over file
        std::env::set_var("AROUTER_TEST_KEY", "from-env");
        assert_eq!(
            ks.resolve(&Some("env:AROUTER_TEST_KEY".into())).as_deref(),
            Some("from-env")
        );
        std::env::remove_var("AROUTER_TEST_KEY");
    }

    // ── callers ──

    fn callers_cfg(require: bool) -> Config {
        let mut cfg = Config::default();
        cfg.auth.require = require;
        cfg.callers = vec![
            CallerConfig {
                budget_usd_day: None,
                name: "alice".into(),
                project: Some("arouter".into()),
                key: Some("key:k-alice".into()),
            },
            CallerConfig {
                budget_usd_day: None,
                name: "ci".into(),
                project: None,
                key: Some("key:k-ci".into()),
            },
        ];
        cfg
    }

    #[test]
    fn callers_parse_and_validate() {
        let cfg = callers_cfg(false);
        cfg.validate().unwrap();
        let map = CallerMap::from_config(&cfg, &KeyStore::default());
        assert_eq!(map.len(), 2);

        // require with no callers → invalid
        let mut cfg = Config::default();
        cfg.auth.require = true;
        assert!(cfg.validate().unwrap_err().contains("[[callers]]"));

        // duplicate / reserved names
        let mut cfg = Config::default();
        cfg.callers = vec![
            CallerConfig {
                budget_usd_day: None,
                name: "a".into(),
                key: None,
                project: None,
            },
            CallerConfig {
                budget_usd_day: None,
                name: "a".into(),
                key: None,
                project: None,
            },
        ];
        assert!(cfg.validate().unwrap_err().contains("duplicate"));
        let mut cfg = Config::default();
        cfg.callers = vec![CallerConfig {
            budget_usd_day: None,
            name: "anonymous".into(),
            key: None,
            project: None,
        }];
        assert!(cfg.validate().unwrap_err().contains("reserved"));
    }

    #[test]
    fn caller_resolution_bearer() {
        let cfg = callers_cfg(false);
        let map = CallerMap::from_config(&cfg, &KeyStore::default());

        let id = map.resolve(Some("k-alice"), None, None).unwrap();
        assert_eq!(id.name, "alice");
        assert_eq!(id.project.as_deref(), Some("arouter"));
        assert_eq!(id.stat_key(), "alice/arouter");

        // open mode: unknown key falls back to anonymous
        assert_eq!(
            map.resolve(Some("nope"), None, None).unwrap(),
            CallerIdentity::anonymous()
        );
        // no key at all → anonymous
        assert!(map.resolve(None, None, None).unwrap().is_anonymous());
    }

    #[test]
    fn caller_resolution_open_headers() {
        let cfg = callers_cfg(false);
        let map = CallerMap::from_config(&cfg, &KeyStore::default());
        let id = map.resolve(None, Some("alice"), Some("website")).unwrap();
        assert_eq!(id.name, "alice");
        assert_eq!(id.project.as_deref(), Some("website"));
        // user header without project
        let id = map.resolve(None, Some("bob"), Some("")).unwrap();
        assert_eq!(id.stat_key(), "bob");
        // empty headers → anonymous
        assert!(map.resolve(None, Some(""), None).unwrap().is_anonymous());
    }

    #[test]
    fn caller_resolution_required_mode() {
        let cfg = callers_cfg(true);
        let map = CallerMap::from_config(&cfg, &KeyStore::default());
        // missing key → 401
        assert!(map.resolve(None, Some("alice"), None).is_err());
        // unknown key → 401 (even with headers)
        assert!(map.resolve(Some("nope"), Some("alice"), None).is_err());
        // valid key → identity
        assert_eq!(map.resolve(Some("k-ci"), None, None).unwrap().name, "ci");
    }

    #[test]
    fn unresolved_key_specs_are_skipped() {
        let mut cfg = Config::default();
        cfg.callers = vec![CallerConfig {
            budget_usd_day: None,
            name: "ghost".into(),
            project: None,
            key: Some("env:AROUTER_KEY_MISSING_FOR_SURE".into()),
        }];
        cfg.validate().unwrap();
        let map = CallerMap::from_config(&cfg, &KeyStore::default());
        assert!(map.is_empty());
    }
}
