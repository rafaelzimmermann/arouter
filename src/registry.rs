//! Model registry: resolves config + key store into runtime models and
//! builds ordered attempt lists per (profile, pool, grade request).
//! Implements docs/routing.md §1–§5 (candidate set, fallback policy,
//! preference ordering) — quota scoring arrives in E3.

use crate::config::{Config, FallbackConfig, KeyStore, ModelConfig};
use crate::glob::glob_any;
use crate::grades::{GradeRequest, Ladder};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub id: String,
    pub upstream_name: String,
    /// Rank in the ladder (0 = best).
    pub grade_rank: usize,
    pub pools: Vec<String>,
    pub context: Option<u64>,
    pub base_url: String,
    pub api_key: Option<String>,
    pub weight: f64,
    pub cost_in: f64,
    pub cost_out: f64,
    /// Blended $/Mtok: (in + 3×out)/4 — typical 1:3 coding mix.
    pub blended_cost: f64,
    pub profiles: Vec<String>,
    /// `manual` | `auto` | `catalog` | `heuristic` | `unknown`.
    pub provenance: String,
}

/// Profile with overlays already merged onto the base routing config.
#[derive(Debug, Clone, Default)]
pub struct ProfileEffective {
    pub description: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub fallback: FallbackConfig,
    pub preference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub ladder: Ladder,
    pub models: Vec<Model>,
    pub profiles: BTreeMap<String, ProfileEffective>,
    pub base_fallback: FallbackConfig,
    /// pool name → preference (`cheapest` | `fastest` | `manual`)
    pub pool_preference: BTreeMap<String, String>,
}

impl Registry {
    pub fn from_config(cfg: &Config, keys: &KeyStore) -> anyhow::Result<Self> {
        cfg.validate().map_err(anyhow::Error::msg)?;
        let ladder = cfg.grades.ladder();

        let models = cfg
            .models
            .iter()
            .map(|m: &ModelConfig| Model {
                id: m.id.clone(),
                upstream_name: m.upstream_name().to_string(),
                grade_rank: ladder.rank(&m.grade).expect("validated"),
                pools: m.pool.clone(),
                context: m.context,
                base_url: m.base_url.trim_end_matches('/').to_string(),
                api_key: keys.resolve(&m.auth),
                weight: if m.weight > 0.0 { m.weight } else { 1.0 },
                cost_in: m.cost_in,
                cost_out: m.cost_out,
                blended_cost: (m.cost_in + 3.0 * m.cost_out) / 4.0,
                profiles: m.profiles.clone(),
                provenance: if m.provenance.is_empty() {
                    "manual".to_string()
                } else {
                    m.provenance.clone()
                },
            })
            .collect();

        let mut profiles = BTreeMap::new();
        for (name, p) in &cfg.profiles {
            let vis = p.models.clone().unwrap_or_default();
            let mut fallback = cfg.routing.fallback.clone();
            if let Some(pf) = &p.fallback {
                // overlay: profile allow keys replace base per-key; a
                // cross_pool that is PRESENT replaces (Some([]) explicitly
                // disables inherited paid fallback — empty ≠ unspecified,
                // review F7), absent inherits the base
                for (k, v) in &pf.allow {
                    fallback.allow.insert(k.clone(), v.clone());
                }
                if let Some(cp) = &pf.cross_pool {
                    fallback.cross_pool = Some(cp.clone());
                }
            }
            profiles.insert(
                name.clone(),
                ProfileEffective {
                    description: p.description.clone(),
                    include: vis.include,
                    exclude: vis.exclude,
                    fallback,
                    preference: p.preference.clone(),
                },
            );
        }

        let pool_preference = cfg
            .pool
            .iter()
            .map(|(k, v)| {
                let pref = match v.preference.as_str() {
                    "fastest" => "fastest",
                    "manual" => "manual",
                    _ => "cheapest-or-manual", // normalized below per pool default
                };
                (k.clone(), pref.to_string())
            })
            .collect();

        Ok(Self {
            ladder,
            models,
            profiles,
            base_fallback: cfg.routing.fallback.clone(),
            pool_preference,
        })
    }

    /// Fallback policy for a profile (merged) or the base config.
    pub fn fallback_for(&self, profile: &str) -> &FallbackConfig {
        if profile == "default" {
            &self.base_fallback
        } else {
            self.profiles
                .get(profile)
                .map(|p| &p.fallback)
                .unwrap_or(&self.base_fallback)
        }
    }

    /// Preference for (profile, pool): profile override → pool config →
    /// default (`manual` for quota/local, `cheapest` for on-demand).
    pub fn preference_for(&self, profile: &str, pool: &str) -> &'static str {
        let explicit = self
            .profiles
            .get(profile)
            .and_then(|p| p.preference.as_deref())
            .or_else(|| self.pool_preference.get(pool).map(|s| s.as_str()));
        match explicit {
            Some("fastest") => "fastest",
            Some("cheapest") => "cheapest",
            Some("manual") => "manual",
            _ if pool == "ondemand" => "cheapest",
            _ => "manual",
        }
    }

    fn visible(&self, model: &Model, profile: &str) -> bool {
        // model-side exclusivity: empty = all profiles
        if !model.profiles.is_empty() && !model.profiles.iter().any(|p| p == profile) {
            return false;
        }
        if let Some(p) = self.profiles.get(profile) {
            if !p.include.is_empty() && !glob_any(&p.include, &model.id) {
                return false;
            }
            if !p.exclude.is_empty() && glob_any(&p.exclude, &model.id) {
                return false;
            }
        }
        true
    }

    /// Number of models visible to (profile, pool) at exactly this grade.
    pub fn count_exact(&self, profile: &str, pool: &str, rank: usize) -> usize {
        self.models
            .iter()
            .filter(|m| {
                m.grade_rank == rank
                    && m.pools.iter().any(|p| p == pool)
                    && self.visible(m, profile)
            })
            .count()
    }

    /// Is any model registered for (profile, pool) at exactly this rank?
    pub fn has_exact(&self, profile: &str, pool: &str, rank: usize) -> bool {
        self.count_exact(profile, pool, rank) > 0
    }

    /// Ordered attempt list (docs/routing.md §5): best grade first, then
    /// within a grade by the pool's preference (cheapest-first for
    /// on-demand), then weight, then id.
    pub fn attempt_list(&self, profile: &str, pool: &str, req: &GradeRequest) -> Vec<&Model> {
        let fb = self.fallback_for(profile);
        let ceiling_token = self.ladder.token(req.ceiling).to_string();
        let ranks =
            self.ladder
                .effective_ranks(req.ceiling, fb.allow.get(&ceiling_token), req.floor);
        let preference = self.preference_for(profile, pool);

        let mut cands: Vec<&Model> = self
            .models
            .iter()
            .filter(|m| m.pools.iter().any(|p| p == pool))
            .filter(|m| ranks.contains(&m.grade_rank))
            .filter(|m| self.visible(m, profile))
            .collect();

        cands.sort_by(|a, b| {
            a.grade_rank
                .cmp(&b.grade_rank)
                .then_with(|| match preference {
                    "cheapest" => a
                        .blended_cost
                        .partial_cmp(&b.blended_cost)
                        .unwrap_or(std::cmp::Ordering::Equal),
                    _ => std::cmp::Ordering::Equal,
                })
                .then_with(|| {
                    b.weight
                        .partial_cmp(&a.weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        cands
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use crate::config::{ModelConfig, ProfileConfig, VisibilityConfig};

    fn ks() -> KeyStore {
        KeyStore::default()
    }

    fn m(id: &str, grade: &str, pools: &[&str], weight: f64, costs: (f64, f64)) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            grade: grade.into(),
            pool: pools.iter().map(|s| s.to_string()).collect(),
            base_url: format!("http://{}.test/v1", id.split('/').next().unwrap()),
            weight,
            cost_in: costs.0,
            cost_out: costs.1,
            ..ModelConfig::default()
        }
    }

    fn reg(models: Vec<ModelConfig>) -> Registry {
        let mut cfg = Config::default();
        cfg.models = models;
        Registry::from_config(&cfg, &ks()).unwrap()
    }

    fn req(l: &Ladder, g: &str) -> GradeRequest {
        l.parse_request(g).unwrap()
    }

    #[test]
    fn delegation_never_upward() {
        let r = reg(vec![
            m("p/opus", "A+", &["quota"], 1.0, (0.0, 0.0)),
            m("p/small", "B", &["quota"], 1.0, (0.0, 0.0)),
        ]);
        // A+ request with default allow [A+, A]: only opus qualifies
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "A+"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["p/opus"]
        );
        // B request never reaches the A+ model
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "B"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["p/small"]
        );
    }

    #[test]
    fn default_allow_one_step_and_ordering() {
        let r = reg(vec![
            m("p/glm", "A", &["quota"], 1.0, (0.0, 0.0)),
            m("p/mini", "B", &["quota"], 9.0, (0.0, 0.0)),
        ]);
        // A request → [A, B] (one-step default); A first even though B has weight 9
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "A"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["p/glm", "p/mini"]
        );
        // weight orders within the same grade
        let r = reg(vec![
            m("p/low", "A", &["quota"], 1.0, (0.0, 0.0)),
            m("p/high", "A", &["quota"], 5.0, (0.0, 0.0)),
        ]);
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "A"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["p/high", "p/low"]
        );
    }

    #[test]
    fn request_range_caps_the_floor() {
        let mut cfg = Config::default();
        // base policy: A+ may drop all the way to B
        cfg.routing
            .fallback
            .allow
            .insert("A+".into(), vec!["A+".into(), "A".into(), "B".into()]);
        cfg.models = vec![
            m("p/opus", "A+", &["quota"], 1.0, (0.0, 0.0)),
            m("p/glm", "A", &["quota"], 1.0, (0.0, 0.0)),
            m("p/mini", "B", &["quota"], 1.0, (0.0, 0.0)),
        ];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        // A+..A stops at A
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "A+..A"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["p/opus", "p/glm"]
        );
    }

    #[test]
    fn pools_are_hard_boundaries() {
        let r = reg(vec![m("p/glm", "A", &["quota"], 1.0, (0.0, 0.0))]);
        assert!(r
            .attempt_list("default", "local", &req(&r.ladder, "A"))
            .is_empty());
        assert!(r
            .attempt_list("default", "ondemand", &req(&r.ladder, "A"))
            .is_empty());
        // model in two pools
        let r = reg(vec![m("p/glm", "A", &["quota", "local"], 1.0, (0.0, 0.0))]);
        assert_eq!(
            r.attempt_list("default", "local", &req(&r.ladder, "A"))
                .len(),
            1
        );
    }

    #[test]
    fn ondemand_is_cheapest_first_within_grade() {
        let r = reg(vec![
            m("or/expensive", "A", &["ondemand"], 9.0, (9.0, 9.0)), // blended 9.0
            m("or/cheap", "A", &["ondemand"], 1.0, (0.2, 0.6)),     // blended 0.5
            m("or/mid", "A", &["ondemand"], 1.0, (2.0, 2.0)),       // blended 2.0
        ]);
        let list = r.attempt_list("default", "ondemand", &req(&r.ladder, "A"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["or/cheap", "or/mid", "or/expensive"]
        );
        // quota pool stays weight-ordered
        let r = reg(vec![
            m("or/expensive", "A", &["quota"], 9.0, (9.0, 9.0)),
            m("or/cheap", "A", &["quota"], 1.0, (0.2, 0.6)),
        ]);
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "A"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["or/expensive", "or/cheap"]
        );
    }

    #[test]
    fn profile_exclusivity_and_globs() {
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "night".into(),
            ProfileConfig {
                models: Some(VisibilityConfig {
                    exclude: vec!["*opus*".into()],
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        cfg.models = vec![
            m("p/opus", "A+", &["quota"], 1.0, (0.0, 0.0)),
            m("p/glm", "A", &["quota"], 1.0, (0.0, 0.0)),
        ];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        // night never sees opus
        let list = r.attempt_list("night", "quota", &req(&r.ladder, "A+"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["p/glm"]
        );
        // default does
        let list = r.attempt_list("default", "quota", &req(&r.ladder, "A+"));
        assert_eq!(list.len(), 2);

        // model-side exclusivity
        let mut cfg = Config::default();
        cfg.profiles.insert("vip".into(), ProfileConfig::default());
        let mut vip_only = m("or/opus", "A+", &["ondemand"], 1.0, (9.0, 9.0));
        vip_only.profiles = vec!["vip".into()];
        cfg.models = vec![vip_only, m("or/cheap", "A", &["ondemand"], 1.0, (0.1, 0.1))];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        // strict A+..A+ request in default profile: opus is invisible, nothing qualifies
        assert!(r
            .attempt_list("default", "ondemand", &req(&r.ladder, "A+..A+"))
            .is_empty());
        // plain A+ may drop one grade: cheap (A) is reachable, opus (A+) never is
        let list = r.attempt_list("default", "ondemand", &req(&r.ladder, "A+"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["or/cheap"]
        );
        // vip sees its exclusive model first
        let list = r.attempt_list("vip", "ondemand", &req(&r.ladder, "A+"));
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["or/opus", "or/cheap"]
        );
    }

    #[test]
    fn profile_fallback_overlay() {
        let mut cfg = Config::default();
        // base: A is strict; night: A may drop to B
        cfg.routing
            .fallback
            .allow
            .insert("A".into(), vec!["A".into()]);
        cfg.profiles.insert(
            "night".into(),
            ProfileConfig {
                fallback: Some(FallbackConfig {
                    allow: [("A".to_string(), vec!["A".to_string(), "B".to_string()])]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        cfg.models = vec![
            m("p/glm", "A", &["quota"], 1.0, (0.0, 0.0)),
            m("p/mini", "B", &["quota"], 1.0, (0.0, 0.0)),
        ];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        assert_eq!(
            r.attempt_list("default", "quota", &req(&r.ladder, "A"))
                .len(),
            1
        );
        assert_eq!(
            r.attempt_list("night", "quota", &req(&r.ladder, "A")).len(),
            2
        );
    }

    #[test]
    fn explicit_empty_cross_pool_disables_inherited_fallback() {
        // review F7: a profile must be able to turn OFF paid fallback;
        // `cross_pool = []` is PRESENT, not unspecified
        let mut cfg = Config::default();
        cfg.routing.fallback.cross_pool = Some(vec!["ondemand".into()]);
        cfg.profiles.insert(
            "strict".into(),
            ProfileConfig {
                fallback: Some(FallbackConfig {
                    cross_pool: Some(vec![]),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        cfg.models = vec![
            m("p/glm", "A+", &["quota"], 1.0, (0.0, 0.0)),
            m("o/expensive", "A", &["ondemand"], 1.0, (2.0, 8.0)),
        ];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        // base policy carries the paid fallback (implicit default profile)
        assert_eq!(
            r.base_fallback.cross_pool,
            Some(vec!["ondemand".to_string()])
        );
        // strict profile explicitly disables it
        assert_eq!(
            r.profiles.get("strict").unwrap().fallback.cross_pool,
            Some(vec![])
        );
    }

    #[test]
    fn api_key_resolution() {
        let mut cfg = Config::default();
        let mut model = m("zai/glm", "A", &["quota"], 1.0, (0.0, 0.0));
        model.auth = Some("key:tok-123".into());
        cfg.models = vec![model];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        assert_eq!(r.models[0].api_key.as_deref(), Some("tok-123"));
    }
}
