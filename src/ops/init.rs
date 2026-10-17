//! E6: `arouter init` — seed the config, merge pi's `models.json`
//! (backup + JSON-aware), install the systemd user unit, then hand off
//! to `doctor`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::{default_config_path, home_dir, Config, KeyStore};
use crate::registry::Registry;

/// One model entry in a pi provider block.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PiModel {
    pub id: String,
    pub name: String,
    #[serde(rename = "contextWindow", skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(rename = "maxTokens")]
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
}

/// Merge our provider entries into an existing models.json document,
/// preserving every other provider. Returns the new JSON value.
pub fn merge_models_doc(
    existing: serde_json::Value,
    entries: BTreeMap<String, Vec<PiModel>>,
    bind: &str,
    api_key: &str,
) -> serde_json::Value {
    let mut root = match existing {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    let providers = root
        .entry("providers".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let providers = match providers {
        serde_json::Value::Object(m) => m,
        _ => {
            *providers = serde_json::json!({});
            providers.as_object_mut().unwrap()
        }
    };
    for (name, models) in &entries {
        let pool_path: String = match name.as_str() {
            "arouter" => "/v1".to_string(),
            "arouter-local" => "/local/v1".to_string(),
            "arouter-ondemand" => "/ondemand/v1".to_string(),
            // profile entries: arouter-<profile>[-ondemand]
            other => {
                if let Some(p) = other.strip_prefix("arouter-") {
                    if let Some(prof) = p.strip_suffix("-ondemand") {
                        format!("/{prof}/ondemand/v1")
                    } else {
                        format!("/{p}/v1")
                    }
                } else {
                    "/v1".to_string()
                }
            }
        };
        providers.insert(
            name.clone(),
            serde_json::json!({
                "api": "openai-completions",
                "apiKey": api_key,
                "baseUrl": format!("http://{bind}{pool_path}"),
                "models": models,
            }),
        );
    }
    // we own every arouter-* entry: ones missing from this merge (deleted
    // pools, removed profiles) are cleaned up; foreign providers untouched
    providers.retain(|name, _| !name.starts_with("arouter") || entries.contains_key(name));
    serde_json::Value::Object(root)
}

/// Build the client-facing routing-group entries for pi's `models.json`:
/// a single `arouter` provider whose models are the config's routing
/// groups (the `[[groups]]` defaults or the user's own list). A client
/// picks a group; arouter's routing strategy picks the upstream.
pub fn group_entries(cfg: &Config) -> Result<BTreeMap<String, Vec<PiModel>>> {
    // resolve membership through the registry so group ladders, per-member
    // overrides, and profile visibility match exactly what the daemon uses
    let reg = Registry::from_config(cfg, &KeyStore::default())?;
    let mut models: Vec<PiModel> = Vec::new();
    for g in &reg.groups {
        for (tok, ctx) in reg.group_grades("default", g) {
            models.push(PiModel {
                id: format!("{}-{}", g.id, tok),
                name: format!("{}-{}", g.id, tok),
                context_window: ctx,
                max_tokens: default_max_tokens(&tok, g.pool.map(|p| p.as_str()).unwrap_or("quota")),
                reasoning: default_reasoning(&tok),
            });
        }
    }
    let mut out = BTreeMap::new();
    out.insert("arouter".to_string(), models);
    Ok(out)
}

fn default_max_tokens(grade: &str, pool: &str) -> u32 {
    match (grade, pool) {
        ("A+", "quota") | ("A", "quota") => 65536,
        ("A+", "local") => 32768,
        ("B", "local") => 16384,
        (_, "quota") | (_, "ondemand") => 65536,
        _ => 32768,
    }
}

fn default_reasoning(grade: &str) -> Option<bool> {
    match grade {
        "A+" | "A" => Some(true),
        _ => None,
    }
}

/// `[server].bind` → loopback HTTP base URL: wildcard hosts resolve to
/// 127.0.0.1 (the daemon listens there); a stray `http://` is tolerated.
fn daemon_base(bind: &str) -> String {
    let bind = bind
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "3929"));
    let host = host.trim_matches(['[', ']']);
    let host = if host.is_empty() || host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    format!("http://{host}:{port}")
}

/// Fetch the live `{group}-{grade}` ids a running daemon advertises on
/// `/v1/models` (F14 sync). `None` = daemon unreachable or auth-refused;
/// init then falls back to the config-derived surface (still correct,
/// just without discovered-only grades).
async fn fetch_live_model_ids(bind: &str) -> Option<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{}/v1/models", daemon_base(bind)))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    Some(
        json.get("data")?
            .as_array()?
            .iter()
            .filter_map(|m| m.get("id")?.as_str().map(String::from))
            .collect(),
    )
}

/// Split a client-facing `{group}-{grade}` id against the config groups:
/// exact bare-group match first (no grade), then `{id}-{grade}` prefix +
/// grade-token validation on that group's ladder (group ids contain `-`,
/// so prefix matching must be ladder-validated to stay unambiguous).
fn split_surface_id<'a>(
    reg: &'a Registry,
    id: &'a str,
) -> Option<(&'a crate::registry::Group, &'a str)> {
    if reg.group(id).is_some() {
        return None; // bare group id — never listed on /v1/models
    }
    for g in &reg.groups {
        if let Some(gp) = id.strip_prefix(&format!("{}-", g.id)) {
            if g.ladder.rank(gp).is_some() {
                return Some((g, gp));
            }
        }
    }
    None
}

/// Enrich live `/v1/models` ids with config-derived metadata (context,
/// maxTokens, reasoning). Config-known grades keep their metadata;
/// discovered-only grades synthesize it from the grade token + group pool.
fn enrich_live_entries(
    reg: &Registry,
    config_models: &[PiModel],
    live_ids: &[String],
) -> Vec<PiModel> {
    let by_id: HashMap<&str, &PiModel> = config_models.iter().map(|m| (m.id.as_str(), m)).collect();
    live_ids
        .iter()
        .map(|id| {
            if let Some(m) = by_id.get(id.as_str()) {
                return (*m).clone();
            }
            let (grade, pool) = split_surface_id(reg, id)
                .map(|(g, tok)| (tok, g.pool.map(|p| p.as_str()).unwrap_or("quota")))
                .unwrap_or(("", "quota"));
            PiModel {
                id: id.clone(),
                name: id.clone(),
                context_window: None,
                max_tokens: default_max_tokens(grade, pool),
                reasoning: default_reasoning(grade),
            }
        })
        .collect()
}

/// The pi picker surface: prefer the RUNNING daemon's `/v1/models` list
/// (authoritative — includes discovered grades), enriched with the
/// config-derived metadata; fall back to the config-derived groups when
/// no daemon answers.
async fn live_or_config_entries(cfg: &Config) -> Result<BTreeMap<String, Vec<PiModel>>> {
    let config_entries = group_entries(cfg)?;
    let config_models = config_entries.get("arouter").cloned().unwrap_or_default();
    let Some(live_ids) = fetch_live_model_ids(&cfg.server.bind).await else {
        return Ok(config_entries);
    };
    if live_ids.is_empty() {
        return Ok(config_entries);
    }
    let reg = Registry::from_config(cfg, &KeyStore::default())?;
    let models = enrich_live_entries(&reg, &config_models, &live_ids);
    let mut out = BTreeMap::new();
    out.insert("arouter".to_string(), models);
    Ok(out)
}

/// systemd user unit text for this install.
pub fn systemd_unit(bin: &Path, config: &Path) -> String {
    format!(
        "# managed by arouter init — edits survive only until the next init\n\
         [Unit]\n\
         Description=arouter — quota-aware LLM router\n\
         After=network-online.target\n\n\
         [Service]\n\
         ExecStart={} serve --config {}\n\
         Restart=on-failure\n\
         RestartSec=2\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        bin.display(),
        config.display()
    )
}

fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let bak = path.with_extension("arouter-bak");
    std::fs::copy(path, &bak).with_context(|| format!("backup {}", bak.display()))?;
    Ok(Some(bak))
}

/// Run the whole init flow. Returns the paths it touched (for messages).
pub struct InitOutcome {
    pub config: PathBuf,
    pub models_json: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub unit: Option<PathBuf>,
    pub bin_installed: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config_path: Option<PathBuf>,
    force: bool,
    install_bin: bool,
    install_unit: bool,
    pi_key: &str,
    example_toml: &str,
) -> Result<InitOutcome> {
    let home = home_dir().context("no $HOME")?;
    let cfg_dir = home.join(".config/arouter");
    std::fs::create_dir_all(&cfg_dir)?;

    // 1. seed the config if missing
    let config = config_path
        .or_else(default_config_path)
        .unwrap_or_else(|| cfg_dir.join("config.toml"));
    if config.exists() && !force {
        // keep it — init is idempotent
    } else {
        if let Some(parent) = config.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config, example_toml)
            .with_context(|| format!("write {}", config.display()))?;
    }

    // 2. merge pi models.json — the pi picker surface mirrors the RUNNING
    // daemon's /v1/models (which includes discovered grades) when one is
    // reachable, else the config-derived groups.
    let cfg = Config::load(Some(&config))?;
    let pi_path = home.join(".pi/agent/models.json");
    let entries = live_or_config_entries(&cfg).await?;
    let (models_json, bak) = {
        let existing = std::fs::read_to_string(&pi_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({}));
        let merged = merge_models_doc(existing, entries, &cfg.server.bind, pi_key);
        let new_text = serde_json::to_string_pretty(&merged)? + "\n";
        let bak = if std::fs::read_to_string(&pi_path).ok().as_deref() != Some(new_text.as_str()) {
            backup(&pi_path)?
        } else {
            None
        };
        if let Some(parent) = pi_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&pi_path, new_text)
            .with_context(|| format!("write {}", pi_path.display()))?;
        (Some(pi_path), bak)
    };

    // 3. binary → ~/.local/bin. Skip when arouter is already running FROM
    // the destination (a self-copy fails with ETXTBSY and used to abort
    // init before the systemd unit was written — review F1); upgrade in
    // place via temp file + rename so a running binary is never written
    // mid-flight.
    let mut bin_installed = None;
    let bin_path = home.join(".local/bin/arouter");
    if install_bin {
        if let Ok(exe) = std::env::current_exe() {
            std::fs::create_dir_all(bin_path.parent().unwrap())?;
            place_binary(&exe, &bin_path)
                .with_context(|| format!("install {} -> {}", exe.display(), bin_path.display()))?;
            bin_installed = Some(bin_path.clone());
        }
    }

    // 4. systemd user unit — ExecStart points at the binary that is
    // actually running (honors a custom install prefix; review F1)
    let unit = if install_unit {
        let unit_dir = home.join(".config/systemd/user");
        std::fs::create_dir_all(&unit_dir)?;
        let unit_path = unit_dir.join("arouter.service");
        let exec = std::env::current_exe()
            .ok()
            .unwrap_or_else(|| bin_path.clone());
        std::fs::write(&unit_path, systemd_unit(&exec, &config))
            .with_context(|| format!("write {}", unit_path.display()))?;
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        Some(unit_path)
    } else {
        None
    };

    Ok(InitOutcome {
        config,
        models_json,
        backup: bak,
        unit,
        bin_installed,
    })
}

/// Install `src` as `dst`, safely: no-op when both name the same file
/// (self-copy would hit ETXTBSY), otherwise copy to a sibling temp file
/// and rename over the destination (atomic on the same filesystem, and
/// a running binary keeps its inode). Returns true when bytes moved.
pub fn place_binary(src: &Path, dst: &Path) -> Result<bool> {
    if same_file(src, dst) {
        return Ok(false);
    }
    let tmp = dst.with_extension("arouter.new");
    std::fs::copy(src, &tmp)
        .with_context(|| format!("copy {} -> {}", src.display(), tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .with_context(|| format!("rename {} -> {}", tmp.display(), dst.display()))?;
    Ok(true)
}

/// Exact same-file test on unix: device + inode.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_binary_is_a_noop_on_same_file() {
        // the process-level contract from the review: installing the
        // running executable onto itself must not fail with ETXTBSY
        let exe = std::env::current_exe().unwrap();
        assert!(!place_binary(&exe, &exe).unwrap(), "self-install no-op");
    }

    #[test]
    fn place_binary_upgrades_via_rename() {
        let dir = std::env::temp_dir().join(format!("arouter-init-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");
        std::fs::write(&src, b"new bytes").unwrap();
        std::fs::write(&dst, b"old bytes").unwrap();
        assert!(place_binary(&src, &dst).unwrap());
        assert_eq!(std::fs::read(&dst).unwrap(), b"new bytes");
        assert!(!dir.join("dst.arouter.new").exists(), "temp cleaned");
        // same path → same file → no-op, never an error
        assert!(!place_binary(&dst, &dst).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unit_honors_the_running_binary_path() {
        // review F1: ExecStart must point at where arouter actually runs
        // from (custom prefixes), not hardcode ~/.local/bin
        let unit = systemd_unit(
            Path::new("/opt/prefix/arouter"),
            Path::new("/x/config.toml"),
        );
        assert!(unit.contains("ExecStart=/opt/prefix/arouter serve"));
    }

    fn cfg_with(grades: &[(&str, &str)], profiles: &[&str]) -> Config {
        let mut models: Vec<crate::config::ModelConfig> = grades
            .iter()
            .map(|&(g, pool)| crate::config::ModelConfig {
                id: format!("x/{g}-{pool}"),
                grade: g.into(),
                pool: vec![pool.into()],
                base_url: "http://127.0.0.1:9/v1".into(),
                ..Default::default()
            })
            .collect();
        let mut profiles_map = std::collections::BTreeMap::new();
        for p in profiles {
            profiles_map.insert(p.to_string(), crate::config::ProfileConfig::default());
            models.push(crate::config::ModelConfig {
                id: format!("x/{p}"),
                grade: "A".into(),
                pool: vec!["ondemand".into()],
                profiles: vec![p.to_string()],
                base_url: "http://127.0.0.1:9/v1".into(),
                ..Default::default()
            });
        }
        Config {
            profiles: profiles_map,
            models,
            ..Config::default()
        }
    }

    #[test]
    fn group_entries_defaults_to_three_groups() {
        let mut cfg = cfg_with(
            &[
                ("A+", "quota"),
                ("A", "quota"),
                ("B", "local"),
                ("A", "ondemand"),
            ],
            &[],
        );
        cfg.models[0].context = Some(272000);
        let entries = group_entries(&cfg).unwrap();
        let ar = &entries["arouter"];
        assert_eq!(
            ar.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![
                "arouter-subscription-A+",
                "arouter-subscription-A",
                "arouter-local-B",
                "arouter-ondemand-A"
            ]
        );
        // per-grade context = max over the group's members at that grade
        assert_eq!(ar[0].context_window, Some(272000));
        assert_eq!(ar[1].context_window, None);
        // reasoning follows the grade token
        assert_eq!(ar[0].reasoning, Some(true)); // A+
        assert_eq!(ar[2].reasoning, None); // B
                                           // maxTokens: quota/ondemand vs local-B caps
        assert_eq!(ar[0].max_tokens, 65536);
        assert_eq!(ar[2].max_tokens, 16384);
        assert_eq!(ar[3].max_tokens, 65536);
    }

    #[test]
    fn group_entries_uses_configured_groups() {
        let mut cfg = cfg_with(&[("A", "quota")], &[]);
        cfg.groups = vec![crate::config::GroupConfig {
            id: "arouter-team".into(),
            pool: None,
            members: vec!["x/A-quota".into()],
            grades: std::collections::BTreeMap::new(),
            ladder: vec![],
            fallback: vec![],
        }];
        let entries = group_entries(&cfg).unwrap();
        let ar = &entries["arouter"];
        assert_eq!(ar.len(), 1, "user-defined groups replace the defaults");
        assert_eq!(ar[0].id, "arouter-team-A");
        assert_eq!(ar[0].max_tokens, 65536); // member-form → quota cap
    }

    #[test]
    fn profile_exclusive_models_stay_out_of_groups() {
        // a profile-only quota model must not leak into the default
        // arouter-subscription group's listing or metadata
        let mut cfg = cfg_with(&[("B", "quota")], &["night"]);
        cfg.models.push(crate::config::ModelConfig {
            id: "x/A-only-night".into(),
            grade: "A".into(),
            pool: vec!["quota".into()],
            profiles: vec!["night".into()],
            context: Some(999999),
            base_url: "http://127.0.0.1:9/v1".into(),
            ..Default::default()
        });
        let entries = group_entries(&cfg).unwrap();
        let ar = &entries["arouter"];
        assert_eq!(
            ar.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["arouter-subscription-B"],
            "profile-only models stay off the default group surface"
        );
        assert_eq!(ar[0].context_window, None);
    }

    #[test]
    fn merge_preserves_foreign_providers() {
        let existing = serde_json::json!({
            "providers": {
                "zai": {"api": "openai-completions", "apiKey": "k", "baseUrl": "https://x", "models": []}
            }
        });
        let mut entries = BTreeMap::new();
        entries.insert(
            "arouter".to_string(),
            vec![PiModel {
                id: "A".into(),
                name: "arouter A".into(),
                context_window: None,
                max_tokens: 65536,
                reasoning: Some(true),
            }],
        );
        let merged = merge_models_doc(existing, entries, "127.0.0.1:3929", "local");
        let provs = merged["providers"].as_object().unwrap();
        assert!(provs.contains_key("zai"), "foreign provider untouched");
        assert_eq!(provs["arouter"]["baseUrl"], "http://127.0.0.1:3929/v1");
        assert_eq!(provs["arouter"]["apiKey"], "local");
        assert_eq!(provs["arouter"]["models"][0]["id"], "A");
        assert!(merged.to_string().contains("openai-completions"));
    }

    #[test]
    fn serialized_entries_never_contain_null() {
        // pi's models.json schema treats optional fields as *missing*,
        // never explicit null — a single contextWindow:null makes pi drop
        // the whole file (regression: E6 first shipped nulls).
        let cfg = cfg_with(&[("A+", "quota")], &[]);
        let mut cfg = cfg;
        cfg.models[0].context = Some(131072);
        let entries = group_entries(&cfg).unwrap();
        let doc = merge_models_doc(
            serde_json::json!({"providers": {}}),
            entries,
            "127.0.0.1:3929",
            "local",
        );
        let text = serde_json::to_string(&doc).unwrap();
        assert!(
            !text.contains(":null") && !text.contains(": null"),
            "got: {text}"
        );
        let ar = &doc["providers"]["arouter"];
        assert!(
            ar["models"][0].get("contextWindow").is_some(),
            "set when known"
        );
    }

    #[test]
    fn merge_overwrites_our_stale_entries_and_cleans_orphans() {
        let existing = serde_json::json!({
            "providers": {
                "arouter": {"api": "old", "models": [{"id": "STALE"}]},
                "arouter-deadprofile": {"api": "old", "models": []}
            }
        });
        let mut entries = BTreeMap::new();
        entries.insert("arouter".to_string(), vec![]);
        let merged = merge_models_doc(existing, entries, "127.0.0.1:3929", "local");
        let provs = merged["providers"].as_object().unwrap();
        let ar = &provs["arouter"];
        assert_eq!(ar["api"], "openai-completions");
        assert!(
            ar["models"].as_array().unwrap().is_empty(),
            "stale entry replaced"
        );
        assert!(
            !provs.contains_key("arouter-deadprofile"),
            "orphaned profile entry cleaned up"
        );
    }

    #[test]
    fn group_merge_collapses_the_provider_explosion() {
        // the old surface had one provider per profile×pool; group merge
        // owns a single `arouter` provider and cleans every other arouter-*
        // entry, leaving foreign providers untouched
        let existing = serde_json::json!({
            "providers": {
                "arouter-vip": {"api": "openai-completions", "models": []},
                "arouter-codex-ondemand": {"api": "openai-completions", "models": []},
                "zai": {"api": "openai-completions", "models": []}
            }
        });
        let mut entries = BTreeMap::new();
        entries.insert(
            "arouter".to_string(),
            vec![PiModel {
                id: "arouter-subscription".into(),
                name: "arouter-subscription".into(),
                context_window: None,
                max_tokens: 65536,
                reasoning: Some(true),
            }],
        );
        let merged = merge_models_doc(existing, entries, "127.0.0.1:3929", "local");
        let provs = merged["providers"].as_object().unwrap();
        assert_eq!(provs.len(), 2, "old arouter-* collapsed, foreign kept");
        assert!(provs.contains_key("arouter"));
        assert!(provs.contains_key("zai"));
        assert_eq!(provs["arouter"]["models"][0]["id"], "arouter-subscription");
        assert_eq!(provs["arouter"]["baseUrl"], "http://127.0.0.1:3929/v1");
    }

    #[test]
    fn unit_text_has_execstart() {
        let u = systemd_unit(
            Path::new("/home/u/.local/bin/arouter"),
            Path::new("/home/u/.config/arouter/config.toml"),
        );
        assert!(u.contains("ExecStart=/home/u/.local/bin/arouter serve --config /home/u/.config/arouter/config.toml"));
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn daemon_base_maps_wildcard_to_loopback() {
        assert_eq!(daemon_base("0.0.0.0:3929"), "http://127.0.0.1:3929");
        assert_eq!(daemon_base("127.0.0.1:3929"), "http://127.0.0.1:3929");
        assert_eq!(daemon_base("http://0.0.0.0:3929"), "http://127.0.0.1:3929");
        assert_eq!(daemon_base("127.0.0.1"), "http://127.0.0.1:3929");
    }

    #[test]
    fn split_surface_id_parses_group_grade_unambiguously() {
        let cfg = cfg_with(&[("A", "quota"), ("A", "ondemand")], &[]);
        let reg = Registry::from_config(&cfg, &KeyStore::default()).unwrap();
        let (g, tok) = split_surface_id(&reg, "arouter-ondemand-A").expect("parses");
        assert_eq!(g.id, "arouter-ondemand");
        assert_eq!(tok, "A");
        assert!(
            split_surface_id(&reg, "arouter-ondemand").is_none(),
            "bare id"
        );
        assert!(
            split_surface_id(&reg, "arouter-ondemand-X").is_none(),
            "bad grade"
        );
        assert!(split_surface_id(&reg, "arouter-subscription").is_none());
        assert!(split_surface_id(&reg, "garbage").is_none());
    }

    #[test]
    fn enrich_live_entries_keeps_config_metadata_and_adds_discovered() {
        let cfg = cfg_with(&[("A+", "quota"), ("A", "ondemand")], &[]);
        let reg = Registry::from_config(&cfg, &KeyStore::default()).unwrap();
        let config_models = group_entries(&cfg).unwrap().remove("arouter").unwrap();
        let live = vec![
            "arouter-subscription-A+".to_string(),
            "arouter-ondemand-A".to_string(),
            "arouter-ondemand-B".to_string(), // discovered-only grade
        ];
        let enriched = enrich_live_entries(&reg, &config_models, &live);
        assert_eq!(
            enriched.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec![
                "arouter-subscription-A+",
                "arouter-ondemand-A",
                "arouter-ondemand-B"
            ]
        );
        // config-known entry keeps its metadata
        assert_eq!(enriched[0].reasoning, Some(true)); // A+
        assert_eq!(enriched[0].max_tokens, 65536);
        // discovered-only entry synthesizes from grade token B + ondemand
        assert_eq!(enriched[2].context_window, None);
        assert_eq!(enriched[2].max_tokens, 65536); // ondemand cap
        assert_eq!(enriched[2].reasoning, None); // B
    }
}
