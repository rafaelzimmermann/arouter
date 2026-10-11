//! E6: `arouter init` — seed the config, merge pi's `models.json`
//! (backup + JSON-aware), install the systemd user unit, then hand off
//! to `doctor`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{default_config_path, home_dir, Config, ModelConfig};

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
pub fn group_entries(cfg: &Config) -> BTreeMap<String, Vec<PiModel>> {
    let mut models: Vec<PiModel> = Vec::new();
    for g in cfg.effective_groups() {
        // members visible on the default profile — profile-specific
        // models stay behind their profile paths, not the group surface
        let members: Vec<&ModelConfig> = cfg
            .models
            .iter()
            .filter(|m| {
                m.profiles.is_empty()
                    && match g.pool.as_deref() {
                        Some(p) => m.pool.iter().any(|mp| mp == p),
                        None => g.members.iter().any(|mid| mid == &m.id),
                    }
            })
            .collect();
        let ctx_max = members.iter().filter_map(|m| m.context).max();
        let reasoning = members
            .iter()
            .any(|m| matches!(m.grade.as_str(), "A+" | "A"))
            .then_some(true);
        models.push(PiModel {
            id: g.id.clone(),
            name: g.id.clone(),
            context_window: ctx_max,
            max_tokens: group_max_tokens(g.pool.as_deref()),
            reasoning,
        });
    }
    let mut out = BTreeMap::new();
    out.insert("arouter".to_string(), models);
    out
}

/// pi's `maxTokens` is the output limit it may request; groups span the
/// whole ladder so use the pool's largest sane cap.
fn group_max_tokens(pool: Option<&str>) -> u32 {
    match pool {
        Some("local") => 32768,
        _ => 65536,
    }
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
pub fn run(
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

    // 2. merge pi models.json
    let cfg = Config::load(Some(&config))?;
    let pi_path = home.join(".pi/agent/models.json");
    let entries = group_entries(&cfg);
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
        let entries = group_entries(&cfg);
        let ar = &entries["arouter"];
        assert_eq!(
            ar.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["arouter-subscription", "arouter-local", "arouter-ondemand"]
        );
        // contextWindow = max over the group's visible members
        assert_eq!(ar[0].context_window, Some(272000));
        assert_eq!(ar[1].context_window, None);
        // reasoning follows the top grades present in each group
        assert_eq!(ar[0].reasoning, Some(true)); // quota has A+
        assert_eq!(ar[1].reasoning, None); // local is B only
        assert_eq!(ar[2].reasoning, Some(true)); // ondemand has A
                                                 // maxTokens: local gets the smaller local cap
        assert_eq!(ar[0].max_tokens, 65536);
        assert_eq!(ar[1].max_tokens, 32768);
        assert_eq!(ar[2].max_tokens, 65536);
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
        let entries = group_entries(&cfg);
        let ar = &entries["arouter"];
        assert_eq!(ar.len(), 1, "user-defined groups replace the defaults");
        assert_eq!(ar[0].id, "arouter-team");
        assert_eq!(ar[0].max_tokens, 65536); // member-form → no local cap
    }

    #[test]
    fn profile_exclusive_models_stay_out_of_groups() {
        // a profile-only quota model must not leak into the default
        // arouter-subscription group's metadata
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
        let entries = group_entries(&cfg);
        let ar = &entries["arouter"];
        let sub = ar.iter().find(|m| m.id == "arouter-subscription").unwrap();
        assert_eq!(sub.context_window, None, "profile-only context excluded");
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
        let entries = group_entries(&cfg);
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
}
