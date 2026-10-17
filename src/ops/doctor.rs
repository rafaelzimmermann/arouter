//! E6: `arouter doctor` — one-shot health report. Checks config, keys,
//! the running daemon, upstream reachability, the quota watcher, and the
//! pi integration files. Pure classification helpers are unit-tested.

use std::path::Path;
use std::time::Duration;

use crate::config::{default_config_path, home_dir, Config, KeyStore};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    pub fn tag(&self) -> &'static str {
        match self {
            Level::Ok => "[ ok ]",
            Level::Warn => "[warn]",
            Level::Fail => "[FAIL]",
        }
    }
    fn merge(self, other: Level) -> Level {
        use Level::*;
        match (self, other) {
            (Fail, _) | (_, Fail) => Fail,
            (Warn, _) | (_, Warn) => Warn,
            _ => Ok,
        }
    }
}

/// Classify an upstream `/models` probe result.
pub fn classify_upstream(status: Option<u16>) -> (Level, String) {
    match status {
        Some(s) if (200..300).contains(&s) => (Level::Ok, format!("HTTP {s}")),
        Some(s) if s == 401 || s == 403 => (
            Level::Warn,
            format!("HTTP {s} (reachable, auth refused — key may be wrong)"),
        ),
        Some(404) => (
            Level::Warn,
            "HTTP 404 (up up, no /models endpoint — fine for OpenAI-compatible stubs)".into(),
        ),
        Some(s) => (Level::Warn, format!("HTTP {s}")),
        None => (Level::Fail, "connection refused / timeout".into()),
    }
}

/// Classify the daemon `/status.json` probe.
pub fn classify_daemon(body: &str) -> Result<(Level, String), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("bad status.json: {e}"))?;
    let n = v["models"].as_array().map(|a| a.len()).unwrap_or(0);
    let ver = v["version"].as_str().unwrap_or("?").to_string();
    let up = v["uptime_secs"].as_u64().unwrap_or(0);
    Ok((Level::Ok, format!("v{ver} · {n} models · up {up}s")))
}

/// Does every model auth spec resolve to a non-empty key in the
/// keystore? Returns (level, detail lines).
pub fn check_keys(cfg: &Config, keys: &KeyStore) -> (Level, Vec<String>) {
    let mut missing: Vec<String> = Vec::new();
    for m in &cfg.models {
        if let Some(spec) = &m.auth {
            let env_like = !spec.starts_with("key:") && spec != "none" && !spec.trim().is_empty();
            if env_like && keys.resolve(&Some(spec.clone())).is_none() {
                missing.push(format!("{} needs {spec:?}", m.id));
            }
        }
    }
    if missing.is_empty() {
        (Level::Ok, vec!["all model auth specs resolve".into()])
    } else {
        (Level::Warn, missing)
    }
}

pub struct Report {
    pub overall: Level,
    pub lines: Vec<(Level, String)>,
}

pub async fn run(config_path: Option<&Path>, bind_override: Option<&str>) -> Report {
    let mut lines: Vec<(Level, String)> = Vec::new();
    let mut overall = Level::Ok;

    // 1. config parses
    let cfg_path = config_path
        .map(Path::to_path_buf)
        .or_else(default_config_path);
    let cfg = match Config::load(cfg_path.as_deref()) {
        Ok(c) => {
            let p = cfg_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "defaults".into());
            let n = c.models.len();
            lines.push((Level::Ok, format!("config {p}: {n} models")));
            c
        }
        Err(e) => {
            lines.push((Level::Fail, format!("config invalid: {e}")));
            return Report {
                overall: Level::Fail,
                lines,
            };
        }
    };

    // 2. keys
    let keys = KeyStore::load_default(cfg.server.env_file.as_deref());
    let (lvl, mut details) = check_keys(&cfg, &keys);
    for d in details.drain(..) {
        lines.push((lvl.clone(), format!("keys: {d}")));
    }
    overall = overall.merge(lvl);

    // 3. env file referenced?
    if let Some(env) = &cfg.server.env_file {
        let p = shellexpand(env);
        if Path::new(&p).exists() {
            lines.push((Level::Ok, format!("env file {p}")));
        } else {
            lines.push((
                Level::Warn,
                format!("env file {p} missing (models may lack keys)"),
            ));
            overall = overall.merge(Level::Warn);
        }
    }

    // 4. daemon /status.json
    let bind = bind_override.unwrap_or(&cfg.server.bind).to_string();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("client");
    let url = format!("http://{bind}/status.json");
    match http.get(&url).send().await {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(body) => match classify_daemon(&body) {
                Ok((lvl, msg)) => {
                    lines.push((lvl, format!("daemon {bind}: {msg}")));
                }
                Err(e) => {
                    lines.push((Level::Warn, format!("daemon {bind}: {e}")));
                    overall = overall.merge(Level::Warn);
                }
            },
            Err(e) => {
                lines.push((Level::Warn, format!("daemon {bind}: body read {e}")));
                overall = overall.merge(Level::Warn);
            }
        },
        Ok(r) => {
            lines.push((Level::Warn, format!("daemon {bind}: HTTP {}", r.status())));
            overall = overall.merge(Level::Warn);
        }
        Err(e) => {
            lines.push((
                Level::Fail,
                format!("daemon {bind}: unreachable ({e}) — is `arouter serve` running?"),
            ));
            overall = overall.merge(Level::Fail);
        }
    }

    // 5. upstreams (distinct base_urls, /models probe)
    let mut seen: Vec<String> = Vec::new();
    for m in &cfg.models {
        if !seen.iter().any(|u| u == &m.base_url) {
            seen.push(m.base_url.clone());
        }
    }
    for base in seen.iter().take(12) {
        let url = format!("{base}/models");
        let status = http
            .get(&url)
            .send()
            .await
            .ok()
            .map(|r| r.status().as_u16());
        let (lvl, msg) = classify_upstream(status);
        lines.push((lvl.clone(), format!("upstream {base}: {msg}")));
        overall = overall.merge(lvl);
    }
    if seen.len() > 12 {
        lines.push((
            Level::Ok,
            format!("… {} more upstreams not probed", seen.len() - 12),
        ));
    }

    // 6. quota intake — what source serves each provider, and does it
    // work? (E3b: native fetchers are the engine; a legacy command stays
    // available as an escape hatch.)
    let mut native: Vec<(String, &str)> = vec![]; // (prefix, spec)
    let mut headers: Vec<String> = vec![];
    let mut seen = std::collections::BTreeSet::new();
    for m in &cfg.models {
        let prefix = m.id.split('/').next().unwrap_or(&m.id).to_string();
        if !seen.insert(prefix.clone()) {
            continue;
        }
        let explicit = cfg.quota.sources.get(&prefix).map(|s| s.as_str());
        if explicit.is_some_and(|s| s == "command" || s.starts_with("command:")) {
            continue;
        }
        let kind = crate::quotaeng::resolve(explicit, &m.base_url, None);
        match kind {
            crate::quotaeng::SourceKind::Native(k) => {
                let spec = match k {
                    crate::quotaeng::NativeKind::OpenRouter => "native:openrouter",
                    crate::quotaeng::NativeKind::Zai => "native:zai",
                };
                native.push((prefix, spec));
            }
            crate::quotaeng::SourceKind::Headers => headers.push(prefix),
            crate::quotaeng::SourceKind::Off => lines.push((
                Level::Warn,
                format!("quota: {prefix} is `off` — routing is health-only for it"),
            )),
            crate::quotaeng::SourceKind::Manual(_) => lines.push((
                Level::Ok,
                format!("quota: {prefix} manual windows (config-pinned)"),
            )),
        }
    }
    if !native.is_empty() {
        lines.push((
            Level::Ok,
            format!(
                "quota: native fetch for {} — poll every {}s",
                native
                    .iter()
                    .map(|(p, s)| format!("{p} ({s})"))
                    .collect::<Vec<_>>()
                    .join(", "),
                cfg.quota.poll_secs
            ),
        ));
    }
    if !headers.is_empty() {
        lines.push((
            Level::Ok,
            format!(
                "quota: header harvest for {} (learned from proxied responses)",
                headers.join(", ")
            ),
        ));
    }
    if let Some(cmd) = cfg.quota.command.as_deref() {
        if !cmd.trim().is_empty() {
            let mut parts = cmd.split_whitespace();
            let prog = parts.next().unwrap_or("");
            let args: Vec<&str> = parts.collect();
            match tokio::time::timeout(
                Duration::from_secs(10),
                tokio::process::Command::new(prog).args(&args).output(),
            )
            .await
            {
                Ok(Ok(out)) if out.status.success() => {
                    let n = String::from_utf8_lossy(&out.stdout).len();
                    lines.push((Level::Ok, format!("quota watcher: ok ({n}B stdout)")));
                }
                Ok(Ok(out)) => {
                    lines.push((
                        Level::Warn,
                        format!(
                            "quota watcher: exit {} — {}",
                            out.status,
                            String::from_utf8_lossy(&out.stderr)
                                .chars()
                                .take(80)
                                .collect::<String>()
                        ),
                    ));
                    overall = overall.merge(Level::Warn);
                }
                Ok(Err(e)) => {
                    lines.push((Level::Warn, format!("quota watcher: spawn failed ({e})")));
                    overall = overall.merge(Level::Warn);
                }
                Err(_) => {
                    lines.push((
                        Level::Warn,
                        "quota watcher: timed out after 10s".to_string(),
                    ));
                    overall = overall.merge(Level::Warn);
                }
            }
        }
    }

    // 7. pi models.json
    if let Some(home) = home_dir() {
        let pi = home.join(".pi/agent/models.json");
        match std::fs::read_to_string(&pi) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => {
                    let provs = v["providers"].as_object().map(|m| m.len()).unwrap_or(0);
                    let ours = v["providers"]["arouter"].is_object();
                    if ours {
                        lines.push((
                            Level::Ok,
                            format!("pi models.json: {provs} providers (arouter present)"),
                        ));
                    } else {
                        lines.push((
                            Level::Warn,
                            "pi models.json: no arouter entry — run `arouter init`".into(),
                        ));
                        overall = overall.merge(Level::Warn);
                    }
                }
                Err(e) => {
                    lines.push((Level::Fail, format!("pi models.json: invalid JSON ({e})")));
                    overall = overall.merge(Level::Fail);
                }
            },
            Err(_) => {
                lines.push((
                    Level::Warn,
                    "pi models.json: not found — run `arouter init` after installing pi".into(),
                ));
                overall = overall.merge(Level::Warn);
            }
        }
        // systemd unit — missing is said out loud, not omitted (review F1);
        // activation state when systemctl is available
        let unit = home.join(".config/systemd/user/arouter.service");
        if unit.exists() {
            let active = std::process::Command::new("systemctl")
                .args(["--user", "is-active", "arouter"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            match active.as_str() {
                "active" => lines.push((
                    Level::Ok,
                    "systemd user unit installed, service active".into(),
                )),
                "" => {
                    lines.push((Level::Ok, "systemd user unit installed".into()));
                }
                state => {
                    lines.push((
                        Level::Warn,
                        format!("systemd unit installed but service is {state} — systemctl --user start arouter"),
                    ));
                    overall = overall.merge(Level::Warn);
                }
            }
        } else {
            lines.push((
                Level::Warn,
                "systemd user unit NOT installed — run `arouter init` to create it".into(),
            ));
            overall = overall.merge(Level::Warn);
        }
    }

    Report { overall, lines }
}

fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = home_dir() {
            return h.join(rest).display().to_string();
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_classification() {
        assert_eq!(classify_upstream(Some(200)).0, Level::Ok);
        assert_eq!(classify_upstream(Some(404)).0, Level::Warn);
        assert_eq!(classify_upstream(Some(401)).0, Level::Warn);
        assert_eq!(classify_upstream(Some(503)).0, Level::Warn);
        assert_eq!(classify_upstream(None).0, Level::Fail);
    }

    #[test]
    fn daemon_classification() {
        let (lvl, msg) =
            classify_daemon(r#"{"version":"0.1.0","models":[{},{},{}],"uptime_secs":12}"#)
                .expect("parses");
        assert_eq!(lvl, Level::Ok);
        assert!(msg.contains("3 models") && msg.contains("v0.1.0"));
        assert!(classify_daemon("not json").is_err());
    }

    #[test]
    fn level_merging() {
        assert_eq!(Level::Ok.merge(Level::Warn), Level::Warn);
        assert_eq!(Level::Warn.merge(Level::Ok), Level::Warn);
        assert_eq!(Level::Warn.merge(Level::Fail), Level::Fail);
        assert_eq!(Level::Fail.merge(Level::Ok), Level::Fail);
        assert_eq!(Level::Ok.merge(Level::Ok), Level::Ok);
    }

    #[test]
    fn key_check_flags_missing_env() {
        let cfg = Config {
            models: vec![crate::config::ModelConfig {
                id: "zai/x".into(),
                grade: "A".into(),
                pool: vec!["quota".into()],
                base_url: "http://127.0.0.1:9/v1".into(),
                auth: Some("env:DEFINITELY_NOT_SET_12345".into()),
                ..Default::default()
            }],
            ..Config::default()
        };
        let keys = KeyStore::load_default(None);
        let (lvl, lines) = check_keys(&cfg, &keys);
        assert_eq!(lvl, Level::Warn);
        assert!(lines.iter().any(|l| l.contains("DEFINITELY_NOT_SET_12345")));
    }
}
