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
    /// Upstream wire: `openai` (chat-completions) | `codex` (E7 native
    /// Codex subscription adapter).
    pub wire: String,
    pub profiles: Vec<String>,
    /// `manual` | `auto` | `catalog` | `heuristic` | `unknown`.
    pub provenance: String,
    /// Outgoing role rewrite (config `[[models]].role_map`), e.g.
    /// `developer` → `system` for upstreams that reject the newer role.
    pub role_map: BTreeMap<String, String>,
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
    /// Client-facing routing groups (`[[groups]]`, F13) — the surface
    /// `/v1/models` lists and group requests route through.
    pub groups: Vec<Group>,
}

/// A resolved routing group: membership + its own ladder + fallback
/// pools. Pool-form groups (`pool = "quota"`) select models dynamically;
/// member-form groups pin model indices with optional per-member ranks.
#[derive(Debug, Clone)]
pub struct Group {
    pub id: String,
    pub ladder: Ladder,
    /// Membership form 1: every model in this pool.
    pub pool: Option<String>,
    /// Membership form 2: (model index, rank on the group ladder) pairs.
    pub members: Vec<(usize, usize)>,
    /// `grades` overrides (model index → rank), either membership form.
    pub grade_overrides: BTreeMap<usize, usize>,
    /// Pools tried, in order, when the group can't serve.
    pub fallback: Vec<String>,
}

impl Registry {
    pub fn from_config(cfg: &Config, keys: &KeyStore) -> anyhow::Result<Self> {
        cfg.validate().map_err(anyhow::Error::msg)?;
        let ladder = cfg.grades.ladder();

        let models: Vec<Model> = cfg
            .models
            .iter()
            .map(|m: &ModelConfig| Model {
                id: m.id.clone(),
                upstream_name: m.upstream_name().to_string(),
                grade_rank: ladder.rank(&m.grade).expect("validated"),
                pools: m.pool.clone(),
                context: m.context,
                base_url: if m.wire_kind() == "codex" && m.base_url.trim().is_empty() {
                    crate::codex::DEFAULT_BASE_URL.to_string()
                } else {
                    m.base_url.trim_end_matches('/').to_string()
                },
                api_key: keys.resolve(&m.auth),
                weight: if m.weight > 0.0 { m.weight } else { 1.0 },
                cost_in: m.cost_in,
                cost_out: m.cost_out,
                blended_cost: (m.cost_in + 3.0 * m.cost_out) / 4.0,
                wire: m.wire_kind().to_string(),
                profiles: m.profiles.clone(),
                provenance: if m.provenance.is_empty() {
                    "manual".to_string()
                } else {
                    m.provenance.clone()
                },
                role_map: m.role_map.clone().unwrap_or_default(),
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

        let groups = cfg
            .effective_groups()
            .iter()
            .map(|g| {
                let gladder = if g.ladder.is_empty() {
                    ladder.clone()
                } else {
                    crate::grades::Ladder::new(g.ladder.clone())
                };
                let members = g
                    .members
                    .iter()
                    .filter_map(|id| cfg.models.iter().position(|cm| &cm.id == id))
                    .map(|i| {
                        let rank = g
                            .grades
                            .get(&cfg.models[i].id)
                            .and_then(|t| gladder.rank(t))
                            .or_else(|| gladder.rank(ladder.token(models[i].grade_rank)))
                            .unwrap_or(gladder.worst());
                        (i, rank)
                    })
                    .collect();
                // grades overrides apply to either membership form
                let grade_overrides = g
                    .grades
                    .iter()
                    .filter_map(|(mid, tok)| {
                        let i = cfg.models.iter().position(|cm| &cm.id == mid)?;
                        Some((i, gladder.rank(tok)?))
                    })
                    .collect();
                Group {
                    id: g.id.clone(),
                    ladder: gladder,
                    pool: g.pool.clone(),
                    members,
                    grade_overrides,
                    fallback: g.fallback.clone(),
                }
            })
            .collect();

        Ok(Self {
            ladder,
            models,
            profiles,
            base_fallback: cfg.routing.fallback.clone(),
            pool_preference,
            groups,
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

    /// Is any model visible to the profile in this pool at all? Drives
    /// the routing-group listing on `/v1/models`.
    pub fn has_pool(&self, profile: &str, pool: &str) -> bool {
        self.models
            .iter()
            .any(|m| m.pools.iter().any(|p| p == pool) && self.visible(m, profile))
    }

    /// Group lookup by client-facing id.
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// A model's rank on a group's ladder: explicit member rank first,
    /// else its global grade token re-ranked on the group ladder.
    fn group_rank(&self, g: &Group, idx: usize) -> Option<usize> {
        if let Some(&(_, r)) = g.members.iter().find(|(i, _)| *i == idx) {
            return Some(r);
        }
        if let Some(&r) = g.grade_overrides.get(&idx) {
            return Some(r);
        }
        g.ladder
            .rank(self.ladder.token(self.models[idx].grade_rank))
    }

    /// Is the model a member of the group (either membership form)?
    fn in_group(&self, g: &Group, idx: usize) -> bool {
        match g.pool.as_deref() {
            Some(p) => self.models[idx].pools.iter().any(|x| x == p),
            None => g.members.iter().any(|(i, _)| *i == idx),
        }
    }

    /// Ordered attempt list for a group request, mirroring
    /// `attempt_list` ordering but on the group ladder/membership:
    /// best grade first, then pool preference (pool-form groups), then
    /// weight, then id. Quota/health ordering happens downstream.
    pub fn group_attempt_list(
        &self,
        profile: &str,
        g: &Group,
        req: &crate::grades::GradeRequest,
    ) -> Vec<&Model> {
        let ranks = g.ladder.ranks_for(req, None);
        let preference = g
            .pool
            .as_deref()
            .and_then(|p| self.pool_preference.get(p).map(|s| s.as_str()));
        let mut idxs: Vec<usize> = (0..self.models.len())
            .filter(|&i| self.in_group(g, i))
            .filter(|&i| self.visible(&self.models[i], profile))
            .filter(|&i| self.group_rank(g, i).is_some_and(|r| ranks.contains(&r)))
            .collect();
        idxs.sort_by(|&a, &b| {
            let ra = self.group_rank(g, a).unwrap_or(usize::MAX);
            let rb = self.group_rank(g, b).unwrap_or(usize::MAX);
            ra.cmp(&rb)
                .then_with(|| match preference {
                    Some("cheapest") => self.models[a]
                        .blended_cost
                        .partial_cmp(&self.models[b].blended_cost)
                        .unwrap_or(std::cmp::Ordering::Equal),
                    _ => std::cmp::Ordering::Equal,
                })
                .then_with(|| {
                    self.models[b]
                        .weight
                        .partial_cmp(&self.models[a].weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| self.models[a].id.cmp(&self.models[b].id))
        });
        idxs.into_iter().map(|i| &self.models[i]).collect()
    }

    /// Does the group have any model visible to the profile? Drives the
    /// `/v1/models` group listing.
    pub fn group_has_models(&self, profile: &str, g: &Group) -> bool {
        match g.pool.as_deref() {
            Some(p) => self.has_pool(profile, p),
            None => (0..self.models.len())
                .any(|i| self.in_group(g, i) && self.visible(&self.models[i], profile)),
        }
    }

    /// Grade tokens of `g`'s ladder that have at least one member visible
    /// to the profile, each with the max context window among members at
    /// that rank (`None` when no member declares one). Drives the per-grade
    /// `/v1/models` listing and pi's group-grade entries.
    pub fn group_grades(&self, profile: &str, g: &Group) -> Vec<(String, Option<u64>)> {
        (0..g.ladder.grades.len())
            .filter_map(|rank| {
                let members: Vec<usize> = (0..self.models.len())
                    .filter(|&i| {
                        self.in_group(g, i)
                            && self.visible(&self.models[i], profile)
                            && self.group_rank(g, i) == Some(rank)
                    })
                    .collect();
                if members.is_empty() {
                    return None;
                }
                let ctx = members.iter().filter_map(|&i| self.models[i].context).max();
                Some((g.ladder.grades[rank].clone(), ctx))
            })
            .collect()
    }

    /// Ordered attempt list (docs/routing.md §5): best grade first, then
    /// within a grade by the pool's preference (cheapest-first for
    /// on-demand), then weight, then id.
    pub fn attempt_list(&self, profile: &str, pool: &str, req: &GradeRequest) -> Vec<&Model> {
        let fb = self.fallback_for(profile);
        let ceiling_token = self.ladder.token(req.ceiling).to_string();
        let ranks = self.ladder.ranks_for(req, fb.allow.get(&ceiling_token));
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
    use crate::config::{GroupConfig, ModelConfig, ProfileConfig, VisibilityConfig};

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
    fn codex_wire_defaults_base_url_and_propagates() {
        // E7: a codex model with no base_url gets the ChatGPT Codex
        // backend; the wire kind rides through to the registry Model.
        let mut cfg = Config::default();
        cfg.models = vec![ModelConfig {
            id: "codex/gpt-6-astra".into(),
            grade: "A+".into(),
            pool: vec!["quota".into()],
            wire: Some("codex".into()),
            ..ModelConfig::default()
        }];
        cfg.validate().unwrap(); // empty base_url is legal for codex
        let reg = Registry::from_config(&cfg, &ks()).unwrap();
        let m = reg
            .models
            .iter()
            .find(|m| m.id == "codex/gpt-6-astra")
            .expect("registered");
        assert_eq!(m.wire, "codex");
        assert_eq!(m.base_url, crate::codex::DEFAULT_BASE_URL);
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

    // ───────────────────────── routing groups (F13) ─────────────────────────

    #[test]
    fn default_groups_one_per_pool_with_local_fallback() {
        let r = reg(vec![
            m("zai/glm-5.3", "A+", &["quota"], 1.0, (0.0, 0.0)),
            m("strata/qwen3.8", "B", &["local"], 1.0, (0.0, 0.0)),
        ]);
        assert_eq!(
            r.groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            vec!["arouter-subscription", "arouter-local", "arouter-ondemand"]
        );
        let sub = r.group("arouter-subscription").unwrap();
        assert_eq!(sub.pool.as_deref(), Some("quota"));
        assert_eq!(sub.fallback, vec!["local".to_string()]);
        assert!(r.group("arouter-local").unwrap().fallback.is_empty());
        // pool form: every model of the pool, visibility respected
        let got: Vec<&str> = r
            .group_attempt_list("default", sub, &sub.ladder.whole_ladder_request())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(got, vec!["zai/glm-5.3"]);
        assert!(r.group_has_models("default", r.group("arouter-local").unwrap()));
        assert!(!r.group_has_models("default", r.group("arouter-ondemand").unwrap()));
    }

    #[test]
    fn member_group_ranks_and_grade_override() {
        let mut cfg = Config::default();
        cfg.models = vec![
            m("y/super", "B", &["ondemand"], 1.0, (1.0, 3.0)),
            m("z/super", "A", &["quota"], 1.0, (0.0, 0.0)),
        ];
        // z-super graded ABOVE y-super inside the group despite its
        // global grade, via the per-member override
        cfg.groups = vec![GroupConfig {
            id: "arouter-custom2".into(),
            pool: None,
            members: vec!["y/super".into(), "z/super".into()],
            grades: [("z/super".to_string(), "A+".to_string())]
                .into_iter()
                .collect(),
            ladder: vec![],
            fallback: vec![],
        }];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        let g = r.group("arouter-custom2").unwrap();
        // whole ladder: z/super (A+) before y/super (B)
        let got: Vec<&str> = r
            .group_attempt_list("default", g, &g.ladder.whole_ladder_request())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(got, vec!["z/super", "y/super"]);
        // pinned A+: only z/super is a member at that group grade
        let got: Vec<&str> = r
            .group_attempt_list("default", g, &g.ladder.parse_request("A+").unwrap())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(got, vec!["z/super"]);
    }

    #[test]
    fn group_ladder_override_and_custom_tokens() {
        let mut cfg = Config::default();
        cfg.models = vec![m("y/super", "A", &["quota"], 1.0, (0.0, 0.0))];
        cfg.groups = vec![GroupConfig {
            id: "arouter-team".into(),
            pool: Some("quota".into()),
            members: vec![],
            grades: [("y/super".to_string(), "F".to_string())]
                .into_iter()
                .collect(),
            // custom ladder + per-member override: y/super sits at the
            // group's F token regardless of its global grade
            ladder: vec!["S".into(), "A".into(), "F".into()],
            fallback: vec![],
        }];
        let r = Registry::from_config(&cfg, &ks()).unwrap();
        let g = r.group("arouter-team").unwrap();
        assert_eq!(g.ladder.list(), "S, A, F");
        // pinned S allows S or one lower (A) — F is two below, excluded
        let got: Vec<&str> = r
            .group_attempt_list("default", g, &g.ladder.parse_request("S").unwrap())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert!(got.is_empty());
        // pinned A allows A or F → y/super serves
        let got: Vec<&str> = r
            .group_attempt_list("default", g, &g.ladder.parse_request("A").unwrap())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(got, vec!["y/super"]);
    }

    #[test]
    fn bare_group_request_spans_the_whole_ladder() {
        // whole-ladder requests must not be clipped by the default
        // "one grade lower" fallback policy (regression: B-only local
        // group served nothing for bare arouter-local)
        let r = reg(vec![m("strata/qwen3.8", "B", &["local"], 1.0, (0.0, 0.0))]);
        let g = r.group("arouter-local").unwrap();
        let got: Vec<&str> = r
            .group_attempt_list("default", g, &g.ladder.whole_ladder_request())
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(got, vec!["strata/qwen3.8"]);
    }
}
