//! E10c — the web-owned overlay (`state_dir/web.toml`, 0600).
//!
//! Field-level merge onto manual config + discovery, per docs/architecture.md:
//! - structure/credentials are manual-only — an overlay entry that tries
//!   to change them on an EXISTING model is rejected (entry skipped,
//!   error surfaced in `/status.json`);
//! - operational switches (`disabled`, `weight`) are overlay-owned and
//!   apply to manual AND discovered models;
//! - additions (id unknown to config and discovery) must carry
//!   `base_url` + `grade` and enter the registry with
//!   `provenance = "overlay"`;
//! - a disabled model is dropped from the runtime snapshot BEFORE it is
//!   ever a candidate (never contacted) and reported via
//!   `/status.json` → `overlay.disabled`;
//! - writes are atomic (tmp + fsync + rename, 0600) and the web layer
//!   dry-runs the merge BEFORE saving — a rejected write leaves the
//!   previous snapshot serving;
//! - quota overrides carry `set_at`/`set_by`/`expires_at` and are merged
//!   as synthetic `manual` samples (see `quota::QuotaWatcher::apply_manual`)
//!   — a stale override never dominates fresher native data.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FILE: &str = "web.toml";

/// Default and maximum lifetime of a quota override (docs/architecture.md: default
/// 24h, capped).
pub const QUOTA_OVERRIDE_DEFAULT_TTL: u64 = 24 * 3600;
pub const QUOTA_OVERRIDE_MAX_TTL: u64 = 7 * 24 * 3600;

pub fn path_for(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<OverlayModel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<QuotaOverride>,
}

/// One overlay entry per model id. `disabled`/`weight` are the
/// operational switches for existing models; `base_url`/`grade`/… are
/// only valid for ADDITIONS (an unknown id).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayModel {
    pub id: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Overlay-owned within-grade weight; `None` keeps the configured
    /// value. Must be > 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    // ── addition-only fields (rejected on existing models) ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// Upstream model name when it differs from the id's model part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_in: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_out: Option<f64>,
}

impl OverlayModel {
    /// The addition-only fields set on this entry?
    pub fn structure_fields_set(&self) -> bool {
        self.base_url.is_some()
            || self.grade.is_some()
            || self.pool.is_some()
            || self.context.is_some()
            || self.auth.is_some()
            || self.name.is_some()
            || self.cost_in.is_some()
            || self.cost_out.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaOverride {
    /// Quota panel name (e.g. "z.ai") — matches `ProviderState.panel`.
    pub provider: String,
    /// Window label ("5h", "weekly", "requests" …).
    pub label: String,
    /// Remaining fraction 0..=1.
    pub pct: f64,
    pub reset_unix: Option<u64>,
    pub set_at: u64,
    pub set_by: String,
    pub expires_at: u64,
}

/// Outcome of merging an overlay onto a config — surfaced through
/// `/status.json` (`overlay.disabled`, `overlay.errors`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diag {
    /// Ids the overlay removed from the runtime snapshot.
    pub disabled: Vec<String>,
    /// Rejected entries (structure edits on existing models, invalid
    /// additions). The rest of the overlay still applied.
    pub errors: Vec<String>,
}

impl Overlay {
    /// Load from `state_dir/web.toml`. A missing file is an empty
    /// overlay (fresh install). A corrupt file yields an empty overlay
    /// plus an error — routing continues on manual config + discovery,
    /// the error is visible in status.
    pub fn load(state_dir: &Path) -> (Overlay, Vec<String>) {
        let path = path_for(state_dir);
        match fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Overlay::default(), vec![]),
            Err(e) => (
                Overlay::default(),
                vec![format!("{}: unreadable ({e})", path.display())],
            ),
            Ok(text) => match toml::from_str::<Overlay>(&text) {
                Ok(o) => (o, vec![]),
                Err(e) => (
                    Overlay::default(),
                    vec![format!("{}: parse error ({e})", path.display())],
                ),
            },
        }
    }

    /// Field-level merge onto `cfg` (already discovery-merged). In
    /// place; rejected entries are skipped and reported, the rest
    /// applies. Pure — no I/O.
    pub fn apply(&self, cfg: &mut crate::config::Config) -> Diag {
        let mut diag = Diag::default();
        // TOTALITY: applying must be a pure function of (base, overlay)
        // — drop previous overlay additions first so removals/re-edits
        // work when re-applied to an already-merged config (the web
        // endpoints rebuild from the running snapshot)
        cfg.models.retain(|m| m.provenance != "overlay");
        let ladder: Vec<String> = cfg
            .grades
            .ladder()
            .list()
            .split(", ")
            .map(str::to_string)
            .collect();
        let mut seen = std::collections::BTreeSet::new();

        for entry in &self.models {
            if entry.id.trim().is_empty() {
                diag.errors.push("overlay: entry with empty id".into());
                continue;
            }
            if !seen.insert(entry.id.clone()) {
                diag.errors
                    .push(format!("overlay: duplicate entry for {}", entry.id));
                continue;
            }
            if let Some(pos) = cfg.models.iter().position(|m| m.id == entry.id) {
                // existing model (manual or discovered): operational
                // switches only
                if entry.structure_fields_set() {
                    diag.errors.push(format!(
                        "overlay: {} is already configured — base_url/grade/pool/\
                         context/auth/name/cost are manual-only (disabled/weight allowed)",
                        entry.id
                    ));
                    continue;
                }
                if let Some(w) = entry.weight {
                    if w <= 0.0 || !w.is_finite() {
                        diag.errors
                            .push(format!("overlay: {} weight must be > 0", entry.id));
                        continue;
                    }
                    cfg.models[pos].weight = w;
                }
                if entry.disabled {
                    cfg.models.remove(pos);
                    diag.disabled.push(entry.id.clone());
                }
            } else {
                // addition: must carry base_url + grade
                let Some(base_url) = entry.base_url.as_deref().map(str::trim) else {
                    diag.errors.push(format!(
                        "overlay: {} is unknown — an addition must carry base_url",
                        entry.id
                    ));
                    continue;
                };
                let Some(grade) = entry.grade.as_deref() else {
                    diag.errors.push(format!(
                        "overlay: {} is unknown — an addition must carry grade",
                        entry.id
                    ));
                    continue;
                };
                let pool = entry.pool.clone().unwrap_or_else(|| vec!["quota".into()]);
                // F3 fail-closed: unpriced ≠ free — on-demand additions
                // must carry both costs
                if pool.iter().any(|p| p == "ondemand")
                    && (entry.cost_in.is_none() || entry.cost_out.is_none())
                {
                    diag.errors.push(format!(
                        "overlay: {} in the ondemand pool needs cost_in and cost_out",
                        entry.id
                    ));
                    continue;
                }
                let m = crate::config::ModelConfig {
                    id: entry.id.clone(),
                    grade: grade.to_string(),
                    base_url: base_url.trim_end_matches('/').to_string(),
                    pool,
                    context: entry.context,
                    auth: entry.auth.clone(),
                    name: entry.name.clone(),
                    weight: entry.weight.unwrap_or(1.0),
                    cost_in: entry.cost_in.unwrap_or(0.0),
                    cost_out: entry.cost_out.unwrap_or(0.0),
                    provenance: "overlay".into(),
                    ..Default::default()
                };
                if let Err(e) = validate_addition(&m, &ladder) {
                    diag.errors.push(format!("overlay: {e}"));
                    continue;
                }
                if entry.disabled {
                    // an addition that is born disabled is just hidden —
                    // reported, never a candidate
                    diag.disabled.push(entry.id.clone());
                } else {
                    cfg.models.push(m);
                }
            }
        }
        diag.disabled.sort();
        diag
    }

    /// Dry-run `apply` against a clone — used by the web layer to
    /// REJECT a write before it ever touches disk (a rejected write
    /// leaves the previous snapshot serving).
    pub fn dry_run(&self, cfg: &crate::config::Config) -> Diag {
        let mut clone = cfg.clone();
        self.apply(&mut clone)
    }

    /// Atomic publish: tmp + fsync + rename (0600), mirroring
    /// `tokens.rs`.
    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        let path = path_for(state_dir);
        let text = toml::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("serialize: {e}"))
        })?;
        let tmp = path.with_extension("toml.tmp");
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        {
            #[cfg(unix)]
            let mut f = {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            };
            #[cfg(not(unix))]
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)
    }
}

fn validate_addition(m: &crate::config::ModelConfig, ladder: &[String]) -> Result<(), String> {
    let (provider, _) =
        m.id.split_once('/')
            .ok_or_else(|| format!("{}: id must be provider/model", m.id))?;
    if provider.is_empty() {
        return Err(format!("{}: empty provider in id", m.id));
    }
    if !ladder.contains(&m.grade) {
        return Err(format!(
            "{}: grade {} not in ladder [{}]",
            m.id,
            m.grade,
            ladder.join(", ")
        ));
    }
    if m.pool.is_empty() {
        return Err(format!("{}: pool must not be empty", m.id));
    }
    for p in &m.pool {
        if !crate::config::POOLS.contains(&p.as_str()) {
            return Err(format!(
                "{}: unknown pool {p:?} (one of {})",
                m.id,
                crate::config::POOLS.join(", ")
            ));
        }
    }
    let url = reqwest::Client::new()
        .get(&m.base_url)
        .build()
        .map_err(|e| format!("{}: base_url invalid ({e})", m.id))?
        .url()
        .clone();
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{}: base_url must be http(s)", m.id));
    }
    Ok(())
}

/// Normalize TTL seconds: default 24h, capped at 7d.
pub fn clamp_ttl(secs: Option<u64>) -> u64 {
    secs.unwrap_or(QUOTA_OVERRIDE_DEFAULT_TTL)
        .min(QUOTA_OVERRIDE_MAX_TTL)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::config::{Config, ModelConfig};

    fn m(id: &str, grade: &str, pool: &[&str]) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            grade: grade.into(),
            pool: pool.iter().map(|s| s.to_string()).collect(),
            base_url: "http://up.example/v1".into(),
            ..ModelConfig::default()
        }
    }

    fn overlay_with(entry: OverlayModel) -> Overlay {
        Overlay {
            models: vec![entry],
            quota: vec![],
        }
    }

    fn entry(id: &str) -> OverlayModel {
        OverlayModel {
            id: id.into(),
            ..Default::default()
        }
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "arouter-overlay-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_is_empty_overlay() {
        let (o, errs) = Overlay::load(Path::new("/nonexistent-arouter-e10c"));
        assert_eq!(o, Overlay::default());
        assert!(errs.is_empty());
    }

    #[test]
    fn disable_drops_and_reports() {
        let mut cfg = Config::default();
        cfg.models = vec![
            m("zai/glm", "A", &["quota"]),
            m("strata/qwen", "B", &["local"]),
        ];
        let o = overlay_with({
            let mut e = entry("zai/glm");
            e.disabled = true;
            e
        });
        let diag = o.apply(&mut cfg);
        assert_eq!(diag.errors.len(), 0);
        assert_eq!(diag.disabled, vec!["zai/glm".to_string()]);
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].id, "strata/qwen");
    }

    #[test]
    fn weight_applies_to_existing_models() {
        let mut cfg = Config::default();
        cfg.models = vec![m("zai/glm", "A", &["quota"])];
        let o = overlay_with({
            let mut e = entry("zai/glm");
            e.weight = Some(0.25);
            e
        });
        let diag = o.apply(&mut cfg);
        assert!(diag.errors.is_empty());
        assert_eq!(cfg.models[0].weight, 0.25);
        assert_eq!(cfg.models[0].base_url, "http://up.example/v1");
    }

    #[test]
    fn structure_edits_on_existing_models_are_rejected() {
        let mut cfg = Config::default();
        cfg.models = vec![m("zai/glm", "A", &["quota"])];
        let o = overlay_with({
            let mut e = entry("zai/glm");
            e.base_url = Some("http://evil.example/v1".into());
            e.grade = Some("A+".into());
            e
        });
        let diag = o.apply(&mut cfg);
        assert_eq!(diag.errors.len(), 1);
        assert!(diag.errors[0].contains("manual-only"), "{:?}", diag.errors);
        // the model is untouched
        assert_eq!(cfg.models[0].base_url, "http://up.example/v1");
        assert_eq!(cfg.models[0].grade, "A");
    }

    #[test]
    fn addition_requires_base_url_and_grade() {
        let mut cfg = Config::default();
        let o = overlay_with(entry("new/model"));
        let diag = o.apply(&mut cfg);
        assert_eq!(diag.errors.len(), 1);
        assert!(diag.errors[0].contains("base_url"));
        assert!(cfg.models.is_empty());

        let o = overlay_with({
            let mut e = entry("new/model");
            e.base_url = Some("http://x.example/v1".into());
            e
        });
        let diag = o.apply(&mut cfg);
        assert!(diag.errors[0].contains("grade"));
    }

    #[test]
    fn valid_addition_enters_with_overlay_provenance() {
        let mut cfg = Config::default();
        let o = overlay_with({
            let mut e = entry("own/gpt-x");
            e.base_url = Some("http://own.example/v1/".into());
            e.grade = Some("A".into());
            e.pool = Some(vec!["quota".into()]);
            e.weight = Some(2.0);
            e
        });
        let diag = o.apply(&mut cfg);
        assert!(diag.errors.is_empty(), "{:?}", diag.errors);
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].provenance, "overlay");
        assert_eq!(cfg.models[0].weight, 2.0);
        // trailing slash trimmed
        assert_eq!(cfg.models[0].base_url, "http://own.example/v1");
    }

    #[test]
    fn addition_grade_must_be_on_the_ladder() {
        let mut cfg = Config::default();
        let o = overlay_with({
            let mut e = entry("own/gpt-x");
            e.base_url = Some("http://own.example/v1".into());
            e.grade = Some("Z9".into());
            e
        });
        let diag = o.apply(&mut cfg);
        assert!(diag.errors[0].contains("ladder"), "{:?}", diag.errors);
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn ondemand_addition_needs_costs() {
        let mut cfg = Config::default();
        let o = overlay_with({
            let mut e = entry("own/gpt-x");
            e.base_url = Some("http://own.example/v1".into());
            e.grade = Some("A".into());
            e.pool = Some(vec!["ondemand".into()]);
            e
        });
        let diag = o.apply(&mut cfg);
        assert!(diag.errors[0].contains("cost"), "{:?}", diag.errors);
    }

    #[test]
    fn born_disabled_addition_is_hidden_not_added() {
        let mut cfg = Config::default();
        let o = overlay_with({
            let mut e = entry("own/gpt-x");
            e.base_url = Some("http://own.example/v1".into());
            e.grade = Some("A".into());
            e.disabled = true;
            e
        });
        let diag = o.apply(&mut cfg);
        assert!(diag.errors.is_empty());
        assert!(cfg.models.is_empty(), "never a candidate");
        assert_eq!(diag.disabled, vec!["own/gpt-x".to_string()]);
    }

    #[test]
    fn duplicate_entries_are_rejected() {
        let mut cfg = Config::default();
        let o = Overlay {
            models: vec![
                {
                    let mut e = entry("zai/glm");
                    e.weight = Some(2.0);
                    e
                },
                {
                    let mut e = entry("zai/glm");
                    e.weight = Some(3.0);
                    e
                },
            ],
            quota: vec![],
        };
        cfg.models = vec![m("zai/glm", "A", &["quota"])];
        let diag = o.apply(&mut cfg);
        assert_eq!(diag.errors.len(), 1);
        // first entry won
        assert_eq!(cfg.models[0].weight, 2.0);
    }

    #[test]
    fn save_and_load_roundtrip_0600() {
        let dir = tmpdir();
        let o = Overlay {
            models: vec![{
                let mut e = entry("zai/glm");
                e.disabled = true;
                e
            }],
            quota: vec![QuotaOverride {
                provider: "z.ai".into(),
                label: "5h".into(),
                pct: 0.42,
                reset_unix: Some(1791800000),
                set_at: 1791700000,
                set_by: "admin-session".into(),
                expires_at: 1791786400,
            }],
        };
        o.save(&dir).unwrap();
        let meta = fs::metadata(path_for(&dir)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        let (loaded, errs) = Overlay::load(&dir);
        assert!(errs.is_empty());
        assert_eq!(loaded, o);
        // no tmp residue
        assert!(!path_for(&dir).with_extension("toml.tmp").exists());
    }

    #[test]
    fn corrupt_file_reports_error_and_routes_on_manual() {
        let dir = tmpdir();
        fs::write(path_for(&dir), "not [ valid toml").unwrap();
        let (o, errs) = Overlay::load(&dir);
        assert_eq!(o, Overlay::default());
        assert_eq!(errs.len(), 1);
        // and applying it changes nothing
        let mut cfg = Config::default();
        cfg.models = vec![m("zai/glm", "A", &["quota"])];
        assert_eq!(o.apply(&mut cfg).errors.len(), 0);
        assert_eq!(cfg.models.len(), 1);
    }

    #[test]
    fn dry_run_does_not_mutate() {
        let mut cfg = Config::default();
        cfg.models = vec![m("zai/glm", "A", &["quota"])];
        let o = overlay_with({
            let mut e = entry("zai/glm");
            e.disabled = true;
            e
        });
        let diag = o.dry_run(&cfg);
        assert_eq!(diag.disabled.len(), 1);
        assert_eq!(cfg.models.len(), 1, "original untouched");
    }

    #[test]
    fn apply_is_total_reapplied_to_a_merged_config() {
        let mut cfg = Config::default();
        cfg.models = vec![
            m("zai/glm", "A", &["quota"]),
            m("deepseek/v4", "B", &["quota"]),
        ];
        let o = overlay_with({
            let mut e = entry("own/new");
            e.base_url = Some("http://127.0.0.1:1/v1".into());
            e.grade = Some("B".into());
            e
        });
        // model entries: [disable glm, add own/new]
        let o = crate::overlay::Overlay {
            models: vec![
                {
                    let mut e = entry("zai/glm");
                    e.disabled = true;
                    e
                },
                o.models.into_iter().next().unwrap(),
            ],
            ..o
        };

        o.apply(&mut cfg);
        assert_eq!(cfg.models.len(), 2); // deepseek + own/new
        assert!(cfg.models.iter().any(|m| m.id == "own/new"));

        // apply the SAME overlay again to the already-merged config —
        // the result must not drift (web rebuilds from the snapshot)
        o.apply(&mut cfg);
        assert_eq!(cfg.models.len(), 2);
        assert!(cfg.models.iter().any(|m| m.id == "own/new"));
        assert!(!cfg.models.iter().any(|m| m.id == "zai/glm"));

        // and an overlay that DROPPED the addition removes it on re-apply
        let o2 = crate::overlay::Overlay {
            models: vec![],
            ..o
        };
        o2.apply(&mut cfg);
        assert!(!cfg.models.iter().any(|m| m.id == "own/new"));
        assert_eq!(cfg.models.len(), 1);
    }

    #[test]
    fn ttl_default_and_cap() {
        assert_eq!(clamp_ttl(None), 24 * 3600);
        assert_eq!(clamp_ttl(Some(60)), 60);
        assert_eq!(clamp_ttl(Some(999_999_999)), 7 * 24 * 3600);
    }
}
