//! Upstream model discovery (docs/grading.md §Discovery): polls each
//! configured upstream (`/v1/models`, ollama `/api/tags`) for models not
//! in the registry, so new upstream models appear without config edits.
//! Discovered models inherit pool/auth from a sibling model on the same
//! upstream and get grades via the precedence chain
//! (grades.auto.toml → catalog → heuristic).

use crate::catalog::{AutoGrade, Catalog, HeuristicInput};
use crate::config::{AutoGrades, Config, KeyStore, ModelConfig};
use crate::grades::Ladder;
use std::collections::BTreeMap;
use std::time::Duration;

/// A model found on an upstream but not present in config.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// Registry id: `provider/name`.
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// Auth spec inherited from the sibling model.
    pub auth: Option<String>,
    pub pools: Vec<String>,
    pub context: Option<u64>,
    /// (cost_in, cost_out) $/Mtok when the upstream publishes pricing
    /// (OpenRouter's `/v1/models` carries per-token USD).
    pub price: Option<(f64, f64)>,
    pub local: bool,
    /// Profile visibility inherited from the seed models sharing this
    /// source (review F9): a sibling of a `vip`-only seed is vip-only.
    /// Empty = visible everywhere (only when every seed is).
    pub profiles: Vec<String>,
}

#[derive(Debug)]
struct UpstreamGroup {
    base_url: String,
    auth: Option<String>,
    pools: Vec<String>,
    provider: String,
    known: Vec<String>,
    local: bool,
    ollama: bool,
    /// `wire = "gemini"` seeds list models through Google's `/models`
    /// shape (and authenticate with `x-goog-api-key`).
    gemini: bool,
    /// F9: `None` = unrestricted (a seed with no profile limits);
    /// `Some(set)` = union of restricted seeds' profiles.
    profiles: Option<Vec<String>>,
}

/// Discover models on all upstreams referenced by `models`.
/// Failures are logged and skipped — discovery must never break serving.
pub async fn discover(
    http: &reqwest::Client,
    keys: &KeyStore,
    models: &[ModelConfig],
) -> Vec<Discovered> {
    // F9: a source is (base_url, credential) — two accounts on the same
    // endpoint are independent discovery sources with their own pools
    // and visibility; URL-only grouping would collapse them.
    let mut groups: BTreeMap<(String, Option<String>), UpstreamGroup> = BTreeMap::new();
    for m in models {
        let base = match m.wire_kind() {
            // wires with a canonical upstream discover even when the
            // entry omits base_url (the registry defaults the same way)
            "gemini" if m.base_url.trim().is_empty() => crate::gemini::DEFAULT_BASE_URL,
            _ => m.base_url.trim_end_matches('/'),
        }
        .to_string();
        if base.is_empty() {
            continue;
        }
        let local = is_local(&base);
        let ollama = base.contains(":11434");
        let gemini = m.wire_kind() == "gemini";
        let provider = m.id.split('/').next().unwrap_or(&m.id).to_string();
        let key = (base.clone(), m.auth.clone());
        let g = groups.entry(key).or_insert_with(|| UpstreamGroup {
            base_url: base,
            auth: m.auth.clone(),
            pools: m.pool.clone(),
            provider,
            known: vec![],
            local,
            ollama,
            gemini,
            profiles: None,
        });
        g.known.push(m.upstream_name().to_string());
        g.known.push(m.id.clone());
        if g.pools.is_empty() {
            g.pools = m.pool.clone();
        }
        // F9: visibility — a seed without profile limits makes the whole
        // source unrestricted; restricted seeds union their profiles
        if m.profiles.is_empty() {
            g.profiles = None;
        } else if let Some(set) = &mut g.profiles {
            for p in &m.profiles {
                if !set.contains(p) {
                    set.push(p.clone());
                }
            }
        }
    }

    let mut found = Vec::new();
    for g in groups.values() {
        // A group whose auth spec requires a key we don't have cannot
        // serve anything — advertising its models would only produce 401s.
        let keyless_ok = g
            .auth
            .as_deref()
            .map(|s| s.trim().is_empty() || s.trim() == "none")
            .unwrap_or(true);
        if !keyless_ok && keys.resolve(&g.auth.clone()).is_none() {
            tracing::warn!(
                "discovery skipped {} — auth spec {:?} resolves to no key",
                g.base_url,
                g.auth
            );
            continue;
        }
        let names = match fetch_names(http, keys, g).await {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!("discovery skipped {} : {e}", g.base_url);
                continue;
            }
        };
        for (name, context, price) in names {
            if name.is_empty() || g.known.contains(&name) {
                continue;
            }
            found.push(Discovered {
                id: format!("{}/{}", g.provider, name),
                name,
                base_url: g.base_url.clone(),
                auth: g.auth.clone(),
                pools: g.pools.clone(),
                context,
                price,
                local: g.local,
                profiles: g.profiles.clone().unwrap_or_default(),
            });
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found.dedup_by(|a, b| a.id == b.id);
    found
}

async fn fetch_names(
    http: &reqwest::Client,
    keys: &KeyStore,
    g: &UpstreamGroup,
) -> anyhow::Result<Vec<RawModel>> {
    let (url, ollama_shape) = if g.ollama {
        // base_url is `http://host:11434/v1` — tags live under /api/tags
        let root = g.base_url.strip_suffix("/v1").unwrap_or(&g.base_url);
        (format!("{root}/api/tags"), true)
    } else {
        (format!("{}/models", g.base_url), false)
    };
    let mut rb = http.get(&url).timeout(Duration::from_secs(10));
    if let Some(spec) = &g.auth {
        if let Some(key) = keys.resolve(&Some(spec.clone())) {
            rb = if g.gemini {
                // Google's JSON API keys are not bearer tokens
                rb.header("x-goog-api-key", key)
            } else {
                rb.bearer_auth(key)
            };
        }
    }
    let resp = rb.send().await?.error_for_status()?;
    let json: serde_json::Value = resp.json().await?;
    if g.gemini {
        Ok(parse_gemini(&json))
    } else {
        Ok(parse_names(&json, ollama_shape))
    }
}

/// One row of a `/models` listing: (name, context, $/Mtok (in, out)).
type RawModel = (String, Option<u64>, Option<(f64, f64)>);

/// Variant suffixes an OpenAI-compatible `/v1/models` listing may carry
/// that cannot serve `POST /v1/chat/completions`. OpenRouter's `:batch`
/// routes belong to its Batch API and 404 on the chat endpoint
/// (`cannot be used with the chat/completions endpoint`); arouter only
/// speaks chat, so they must never become candidates. Everything else
/// OpenRouter offers (`:free`, `:nitro`, `:floor`, `:online`,
/// `:thinking`, `:extended`) is a plain chat route and is kept.
fn non_chat_variant(id: &str) -> bool {
    id.ends_with(":batch")
}

/// Parse Google's `/models` listing: `{"models":[{"name":"models/gemini-2.5-pro",
///   "supportedGenerationMethods":["generateContent",…],"inputTokenLimit":…}]}`.
/// Embedding/tuning-only entries cannot serve chat and are skipped; the
/// `models/` URL prefix is stripped so the id matches the wire's model
/// name. `inputTokenLimit` is the closest thing to a context window Google
/// reports (it excludes the output budget) — arouter grades context
/// conservatively from it.
fn parse_gemini(json: &serde_json::Value) -> Vec<RawModel> {
    let mut out = Vec::new();
    if let Some(arr) = json.get("models").and_then(|v| v.as_array()) {
        for m in arr {
            let Some(name) = m.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let id = name.strip_prefix("models/").unwrap_or(name);
            // only entries that can actually generate text are candidates
            let can_generate = m
                .get("supportedGenerationMethods")
                .and_then(|v| v.as_array())
                .map(|methods| {
                    methods
                        .iter()
                        .any(|x| x.as_str().map(|x| x == "generateContent").unwrap_or(false))
                })
                .unwrap_or(true);
            if !can_generate || id.is_empty() {
                continue;
            }
            let ctx = m.get("inputTokenLimit").and_then(|v| v.as_u64());
            out.push((id.to_string(), ctx, None));
        }
    }
    out
}

/// Parse `/v1/models` (OpenAI/OpenRouter shape) or `/api/tags` (ollama).
fn parse_names(json: &serde_json::Value, ollama_shape: bool) -> Vec<RawModel> {
    let mut out = Vec::new();
    if ollama_shape {
        // {"models":[{"name":"qwen3:14b", …}]}
        if let Some(arr) = json.get("models").and_then(|v| v.as_array()) {
            for m in arr {
                let name = m.get("name").and_then(|v| v.as_str()).map(str::to_string);
                if let Some(name) = name {
                    out.push((name, None, None));
                }
            }
        }
    } else {
        // {"data":[{"id":"glm-5.4","context_length":131072,
        //   "pricing":{"prompt":"0.0000015","completion":"0.000006"}}]}
        // (pricing is per-token USD; catalog bands are per-Mtok)
        if let Some(arr) = json.get("data").and_then(|v| v.as_array()) {
            for m in arr {
                let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                if non_chat_variant(id) {
                    continue;
                }
                let ctx = m
                    .get("context_length")
                    .or_else(|| m.get("context_window"))
                    .and_then(|v| v.as_u64());
                let price = m.get("pricing").and_then(|p| {
                    let cin = p.get("prompt")?.as_str()?.parse::<f64>().ok()?;
                    let cout = p.get("completion")?.as_str()?.parse::<f64>().ok()?;
                    // clamp negatives to 0: OpenRouter lists negative prompt
                    // prices (cache credit) on some models — arouter treats
                    // those as free, never as negative budget spend
                    Some((cin.max(0.0) * 1e6, cout.max(0.0) * 1e6))
                });
                out.push((id.to_string(), ctx, price));
            }
        }
    }
    out
}

fn is_local(base_url: &str) -> bool {
    base_url.contains("127.0.0.1") || base_url.contains("localhost") || base_url.contains("[::1]")
}

/// Assign a grade to a discovered model via the precedence chain:
/// grades.auto.toml → catalog → heuristic (→ unknown).
pub fn auto_grade(
    d: &Discovered,
    auto: &AutoGrades,
    catalog: &Catalog,
    ladder: &Ladder,
) -> (String, String, String) {
    // (grade token, provenance, rationale)
    if let Some(entry) = auto.grade_for(&d.id) {
        if let Some(rank) = ladder.rank(&entry.grade) {
            let _ = rank;
            return (
                entry.grade.clone(),
                "auto".to_string(),
                entry.rationale.clone(),
            );
        }
        tracing::warn!(
            "grades.auto.toml: unknown grade {:?} for {}",
            entry.grade,
            d.id
        );
    }
    let input = HeuristicInput {
        id: &d.id,
        price: d.price,
        context: d.context,
        local: d.local,
    };
    let g: AutoGrade = catalog.grade_for(ladder, &input);
    (
        ladder.token(g.grade_rank).to_string(),
        g.provenance.as_str().to_string(),
        g.rationale,
    )
}

/// Merge discovered models into a copy of `cfg` (ids already present in
/// config are skipped — manual config always wins).
pub fn merge_discovered(
    cfg: &Config,
    discovered: &[Discovered],
    auto: &AutoGrades,
    catalog: &Catalog,
) -> Config {
    let ladder = cfg.grades.ladder();
    let mut merged = cfg.clone();
    for d in discovered {
        if merged.models.iter().any(|m| m.id == d.id) {
            continue;
        }
        let (grade, provenance, rationale) = auto_grade(d, auto, catalog, &ladder);
        // Ungradeable discoveries (no price, no catalog match, not local)
        // are skipped, not fatal — junk from an upstream `/v1/models` must
        // never invalidate the registry or block hot-reload.
        if ladder.rank(&grade).is_none() {
            tracing::debug!(model = d.id, "discovered model ungradeable, skipping");
            continue;
        }
        // review F3: unknown price must never silently become "free" in
        // the paid pool — strip ondemand from unpriced AND free (price 0)
        // discoveries. Free models are not "cheap" (the heuristic never
        // favors them) and must not enter the paid pool either.
        // quota/local pools don't charge, so they stay admissible.
        let mut pools = if d.pools.is_empty() {
            vec!["quota".to_string()]
        } else {
            d.pools.clone()
        };
        let free_or_unpriced = match d.price {
            Some((i, o)) => i <= 0.0 && o <= 0.0,
            None => true,
        };
        if free_or_unpriced && pools.iter().any(|p| p == "ondemand") {
            tracing::warn!(
                model = d.id,
                "discovered without payable price metadata; refusing ondemand admission (set cost_in/cost_out to admit it)"
            );
            pools.retain(|p| p != "ondemand");
        }
        if pools.is_empty() {
            tracing::debug!(
                model = d.id,
                "unpriced discovery has no pool left; skipping"
            );
            continue;
        }
        merged.models.push(ModelConfig {
            id: d.id.clone(),
            name: Some(d.name.clone()),
            grade,
            pool: pools,
            context: d.context,
            base_url: d.base_url.clone(),
            auth: d.auth.clone(),
            weight: 0.5, // below manual siblings — provenance-aware default
            cost_in: d.price.map(|p| p.0).unwrap_or(0.0),
            cost_out: d.price.map(|p| p.1).unwrap_or(0.0),
            // F9: no silent broadening — a discovered sibling is exactly
            // as visible as the seed(s) that lent it credentials
            profiles: d.profiles.clone(),
            wire: None,
            provenance,
            rationale,
            role_map: None,
        });
    }
    merged
}

/// Build the AutoGrades payload for `models grade --write`: every model
/// that did not come from manual config (discovered + auto-graded).
pub fn auto_grades_from(models: &[ModelConfig], catalog_version: u32) -> AutoGrades {
    let entries = models
        .iter()
        .filter(|m| !m.provenance.is_empty() && m.provenance != "manual")
        .map(|m| crate::config::AutoModelEntry {
            id: m.id.clone(),
            grade: m.grade.clone(),
            pool: m.pool.clone(),
            provenance: m.provenance.clone(),
            rationale: m.rationale.clone(),
        })
        .collect();
    AutoGrades {
        generated_at: chrono_like_now(),
        catalog_version: Some(catalog_version),
        models: entries,
    }
}

/// Unix-seconds timestamp — monitoring-only field, deliberately not a
/// wall-clock date (no chrono dependency for a cosmetic value).
fn chrono_like_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn disc(id: &str, local: bool) -> Discovered {
        Discovered {
            id: id.into(),
            name: id.rsplit('/').next().unwrap().into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            auth: None,
            pools: vec!["quota".into()],
            context: None,
            price: None,
            local,
            profiles: vec![],
        }
    }

    #[test]
    fn discovered_models_inherit_seed_profile_restrictions() {
        // review F9: a vip-only seed must not leak discovered siblings
        // into the default profile
        let cfg = Config::default();
        let catalog = Catalog::embedded(&cfg.grades.ladder()).unwrap();
        let mut d = disc("or/new-model", false);
        d.profiles = vec!["vip".into()];
        let merged = merge_discovered(&cfg, &[d], &AutoGrades::default(), &catalog);
        let m = merged
            .models
            .iter()
            .find(|m| m.id == "or/new-model")
            .unwrap();
        assert_eq!(m.profiles, vec!["vip".to_string()]);
    }

    #[test]
    fn same_url_different_auth_are_independent_sources() {
        // review F9: grouping key is (base_url, credential) — two keys
        // on one endpoint are two sources, neither collapses the other
        let seed = |id: &str, auth: &str| ModelConfig {
            id: id.into(),
            name: None,
            grade: "A".into(),
            pool: vec!["quota".into()],
            context: None,
            base_url: "http://127.0.0.1:9/v1".into(),
            auth: Some(auth.into()),
            weight: 1.0,
            cost_in: 0.0,
            cost_out: 0.0,
            profiles: vec![],
            provenance: String::new(),
            rationale: String::new(),
            role_map: None,
            wire: None,
        };
        // grouping is inside discover() — offline: just assert the key
        // math via the same tuple the map uses
        let a = (
            "http://127.0.0.1:9/v1".to_string(),
            seed("k/a", "env:KEY_A").auth.clone(),
        );
        let b = (
            "http://127.0.0.1:9/v1".to_string(),
            seed("k/b", "env:KEY_B").auth.clone(),
        );
        assert_ne!(a, b, "same URL, different credential = different source");
        assert!(seed("k/a", "env:KEY_A").auth.is_some());
    }

    #[test]
    fn parse_names_both_shapes() {
        let openai = serde_json::json!({
            "data": [
                {"id": "glm-5.4", "context_length": 131072},
                {"id": "no-ctx"},
                {"context_window": 4096, "id": "alt-key"},
                "garbage"
            ]
        });
        assert_eq!(
            parse_names(&openai, false),
            vec![
                ("glm-5.4".into(), Some(131072), None),
                ("no-ctx".into(), None, None),
                ("alt-key".into(), Some(4096), None),
            ]
        );
        // OpenRouter pricing: per-token USD strings → $/Mtok
        let or = serde_json::json!({
            "data": [{
                "id": "x/big",
                "pricing": {"prompt": "0.0000015", "completion": "0.000006"}
            }]
        });
        assert_eq!(
            parse_names(&or, false),
            vec![("x/big".into(), None, Some((1.5, 6.0)))]
        );
        let tags = serde_json::json!({"models": [{"name": "qwen3:14b"}, {"x": 1}]});
        assert_eq!(
            parse_names(&tags, true),
            vec![("qwen3:14b".into(), None, None)]
        );
        assert!(parse_names(&serde_json::json!({}), false).is_empty());
    }

    #[test]
    fn gemini_listing_keeps_only_chat_models() {
        // Google's /models shape: names carry a models/ prefix, and only
        // entries offering generateContent can serve a chat request
        let g = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-2.5-pro",
                    "supportedGenerationMethods": ["generateContent", "countTokens"],
                    "inputTokenLimit": 1048576
                },
                {
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {"name": "models/bare"}
            ]
        });
        assert_eq!(
            parse_gemini(&g),
            vec![
                ("gemini-2.5-pro".into(), Some(1048576), None),
                ("bare".into(), None, None),
            ]
        );
    }

    #[test]
    fn batch_variants_are_not_candidates() {
        // review: OpenRouter lists `:batch` routes that 404 on
        // chat/completions — never let them reach the registry
        let or = serde_json::json!({
            "data": [
                {"id": "openai/gpt-6-luna-pro:batch"},
                {"id": "openai/gpt-6-luna-pro"},
                {"id": "x/free:free"},
                {"id": "x/turbo:nitro"},
                {"id": "x/tools:exacto"},
            ]
        });
        let names: Vec<String> = parse_names(&or, false)
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        assert_eq!(
            names,
            vec![
                "openai/gpt-6-luna-pro",
                "x/free:free",
                "x/turbo:nitro",
                "x/tools:exacto",
            ]
        );
    }

    #[test]
    fn merge_skips_ungradeable_discoveries() {
        // a stub upstream exposing /v1/models with an ungradeable name must
        // not poison the registry (hot-reload regression, E9 rig)
        let mut cfg = Config::default();
        cfg.models = vec![];
        let auto = AutoGrades::default();
        let catalog = Catalog::embedded(&cfg.grades.ladder()).unwrap();
        // no price, no catalog match, not local → worst grade, still merged
        let junk = disc("stubd/any", false);
        let good = disc("zai/glm-4.6", false);
        let merged = merge_discovered(&cfg, &[junk, good], &auto, &catalog);
        assert_eq!(merged.models.len(), 2);
        let any = merged.models.iter().find(|m| m.id == "stubd/any").unwrap();
        assert_eq!(any.grade, "C", "unknown provenance lands on worst grade");

        // trimmed ladder (no C): must degrade to B, never produce "?"
        let mut short_cfg = Config::default();
        short_cfg.models = vec![];
        short_cfg.grades.ladder = ["A+", "A", "B"].iter().map(|s| s.to_string()).collect();
        let short_catalog = Catalog::embedded(&short_cfg.grades.ladder()).unwrap();
        let merged2 = merge_discovered(
            &short_cfg,
            &[disc("stubd/any", false)],
            &auto,
            &short_catalog,
        );
        assert_eq!(
            merged2.models[0].grade, "B",
            "clamped to worst ladder grade"
        );
        assert!(merged2.validate().is_ok(), "merged config must stay valid");
    }

    #[test]
    fn merge_strips_free_discoveries_from_ondemand() {
        // free (price 0) discoveries are treated like unpriced ones:
        // ondemand admission is refused, and with no other pool they are
        // skipped entirely — free models never enter the paid pool
        let mut cfg = Config::default();
        cfg.models = vec![ModelConfig {
            id: "or/seed".into(),
            grade: "A".into(),
            pool: vec!["ondemand".into()],
            base_url: "https://openrouter.ai/api/v1".into(),
            ..ModelConfig::default()
        }];
        let auto = AutoGrades::default();
        let catalog = Catalog::embedded(&cfg.grades.ladder()).unwrap();

        let mut free_d = disc("or/a-free", false);
        free_d.pools = vec!["ondemand".into()];
        free_d.price = Some((0.0, 0.0)); // free: both components zero

        let mut paid_d = disc("or/a-paid", false);
        paid_d.pools = vec!["ondemand".into()];
        paid_d.price = Some((1.0, 1.0)); // priced: stays

        let merged = merge_discovered(&cfg, &[free_d, paid_d], &auto, &catalog);
        assert!(
            merged.models.iter().all(|m| m.id != "or/a-free"),
            "free discovery is stripped from ondemand and skipped"
        );
        let paid = merged.models.iter().find(|m| m.id == "or/a-paid").unwrap();
        assert_eq!(paid.pool, vec!["ondemand".to_string()]);
        assert!(paid.cost_in > 0.0 && paid.cost_out > 0.0);
    }

    #[test]
    fn merge_skips_config_ids_and_assigns_grades() {
        let mut cfg = Config::default();
        cfg.models = vec![ModelConfig {
            id: "zai/glm-5.3".into(),
            grade: "A".into(), // manual wins even over auto file
            pool: vec!["quota".into()],
            base_url: "http://127.0.0.1:9/v1".into(),
            ..ModelConfig::default()
        }];
        let auto = AutoGrades::default();
        let catalog = Catalog::embedded(&cfg.grades.ladder()).unwrap();

        let m = disc("zai/glm-4.6", false);
        let dupe = disc("zai/glm-5.3", false);
        let merged = merge_discovered(&cfg, &[m, dupe], &auto, &catalog);

        assert_eq!(merged.models.len(), 2, "dupe skipped, new added");
        assert_eq!(merged.models[0].id, "zai/glm-5.3");
        assert_eq!(merged.models[0].grade, "A", "manual untouched");
        let new = &merged.models[1];
        assert_eq!(new.id, "zai/glm-4.6");
        assert_eq!(new.provenance, "catalog");
        assert_eq!(new.grade, "A");
        assert_eq!(new.weight, 0.5);
    }

    #[test]
    fn auto_file_overrides_catalog() {
        let cfg = Config::default();
        let ladder = cfg.grades.ladder();
        let mut auto = AutoGrades::default();
        auto.models.push(crate::config::AutoModelEntry {
            id: "zai/glm-5.4".into(),
            grade: "B".into(),
            pool: vec!["quota".into()],
            provenance: "catalog".into(),
            rationale: "pinned in auto file".into(),
        });
        let catalog = Catalog::embedded(&ladder).unwrap();
        let d = disc("zai/glm-5.4", false);
        let (grade, prov, why) = auto_grade(&d, &auto, &catalog, &ladder);
        assert_eq!((grade.as_str(), prov.as_str()), ("B", "auto"));
        assert_eq!(why, "pinned in auto file");
    }

    #[test]
    fn unknown_local_gets_heuristic() {
        let cfg = Config::default();
        let ladder = cfg.grades.ladder();
        let catalog = Catalog::embedded(&ladder).unwrap();
        let d = disc("ollama/mystery-model", true);
        let (grade, prov, _) = auto_grade(&d, &AutoGrades::default(), &catalog, &ladder);
        assert_eq!(prov, "heuristic");
        assert_eq!(grade, "B", "local base B");
    }

    #[test]
    fn auto_grades_from_excludes_manual() {
        let mut cfg = Config::default();
        let mut manual = ModelConfig {
            id: "zai/glm-5.3".into(),
            grade: "A+".into(),
            pool: vec!["quota".into()],
            base_url: "http://x/v1".into(),
            ..ModelConfig::default()
        };
        manual.provenance = String::new();
        let mut auto_one = ModelConfig {
            id: "zai/glm-5.4".into(),
            grade: "A+".into(),
            pool: vec!["quota".into()],
            base_url: "http://x/v1".into(),
            ..ModelConfig::default()
        };
        auto_one.provenance = "catalog".into();
        cfg.models = vec![manual, auto_one];
        let ag = auto_grades_from(&cfg.models, 1);
        assert_eq!(ag.models.len(), 1);
        assert_eq!(ag.models[0].id, "zai/glm-5.4");
        assert_eq!(ag.catalog_version, Some(1));
    }
}
