//! Auto-grading: embedded catalog (`src/base/catalog.toml`) + conservative
//! defaults for unknown models (docs/grading.md). Manual
//! `[[models]]` grades in config.toml always win; this module only
//! grades *discovered* models (and powers `arouter models grade`).

use crate::glob::glob_match;
use crate::grades::Ladder;
use serde::Deserialize;

pub const CATALOG_TOML: &str = include_str!("catalog.toml");

#[derive(Debug, Deserialize)]
struct RawCatalog {
    catalog_version: u32,
    #[serde(default)]
    entry: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
struct RawEntry {
    grade: String,
    /// `match` is a keyword; serde-rename keeps the TOML key.
    #[serde(rename = "match", default)]
    matches_: Vec<String>,
    #[serde(default)]
    notes: String,
}

/// A graded catalog entry after parsing.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub grade_rank: usize,
    pub patterns: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub version: u32,
    /// Ordered: first matching entry wins.
    pub entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Catalog,
    Heuristic,
    Unknown,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Catalog => "catalog",
            Provenance::Heuristic => "heuristic",
            Provenance::Unknown => "unknown",
        }
    }
}

/// The outcome of auto-grading one model.
#[derive(Debug, Clone)]
pub struct AutoGrade {
    pub grade_rank: usize,
    pub provenance: Provenance,
    pub rationale: String,
}

impl Catalog {
    pub fn parse(toml: &str, ladder: &Ladder) -> anyhow::Result<Self> {
        let raw: RawCatalog = toml::from_str(toml)?;
        let mut entries = Vec::with_capacity(raw.entry.len());
        let mut skipped = 0usize;
        for e in &raw.entry {
            // A user may trim the ladder below the catalog's grades — skip
            // those entries instead of invalidating the whole catalog (a
            // degraded reload must never cost hot-reload entirely).
            let Some(rank) = ladder.rank(&e.grade) else {
                skipped += 1;
                continue;
            };
            entries.push(CatalogEntry {
                grade_rank: rank,
                patterns: e.matches_.clone(),
                notes: e.notes.clone(),
            });
        }
        if skipped > 0 {
            tracing::debug!(
                skipped,
                "catalog entries with grades outside the ladder ignored"
            );
        }
        Ok(Self {
            version: raw.catalog_version,
            entries,
        })
    }

    /// The embedded catalog, parsed against `ladder`.
    pub fn embedded(ladder: &Ladder) -> anyhow::Result<Self> {
        Self::parse(CATALOG_TOML, ladder)
    }

    /// Match a model id against the catalog. Both the full id
    /// (`or/z-ai/glm-5.3`) and the last segment (`glm-5.3`) are tried.
    pub fn lookup(&self, id: &str) -> Option<&CatalogEntry> {
        let name = id.rsplit('/').next().unwrap_or(id);
        self.entries.iter().find(|e| {
            e.patterns
                .iter()
                .any(|p| glob_match(p, id) || glob_match(p, name))
        })
    }

    /// Full precedence for a *discovered* model: catalog → heuristic.
    pub fn grade_for(&self, ladder: &Ladder, input: &HeuristicInput) -> AutoGrade {
        if let Some(e) = self.lookup(input.id) {
            return AutoGrade {
                grade_rank: e.grade_rank,
                provenance: Provenance::Catalog,
                rationale: if e.notes.is_empty() {
                    "matched catalog entry".to_string()
                } else {
                    format!("catalog: {}", e.notes)
                },
            };
        }
        self.heuristic_grade(ladder, input)
    }

    /// Conservative fallback grading; pricing is used only for cost routing.
    pub fn heuristic_grade(&self, ladder: &Ladder, input: &HeuristicInput) -> AutoGrade {
        // Price and context capacity do not establish response quality.
        // Unknown remote models stay in the lowest tier until a catalog
        // entry or an explicit user grade supplies capability evidence.
        if !input.local {
            return AutoGrade {
                grade_rank: ladder.worst(),
                provenance: Provenance::Unknown,
                rationale: "unvalidated model; price and context do not establish quality".into(),
            };
        }
        // Preserve a conservative local default, with a lower tier for very
        // small models. Never infer a premium grade from marketing names.
        let name = input
            .id
            .rsplit('/')
            .next()
            .unwrap_or(input.id)
            .to_lowercase();
        let tiny = parse_size_billions(&name).is_some_and(|size| size <= 3.0);
        AutoGrade {
            grade_rank: if tiny {
                ladder.worst()
            } else {
                ladder.rank("B").unwrap_or_else(|| ladder.worst())
            },
            provenance: Provenance::Heuristic,
            rationale: if tiny {
                "local model ≤ 3B; conservative lowest tier"
            } else {
                "local model; provisional B tier (explicit grade required for premium tiers)"
            }
            .into(),
        }
    }
}

/// Input for heuristic grading.
#[derive(Debug, Clone)]
pub struct HeuristicInput<'a> {
    pub id: &'a str,
    /// (cost_in, cost_out) $/Mtok; `None` = no data.
    pub price: Option<(f64, f64)>,
    pub context: Option<u64>,
    /// Local engines receive a conservative provisional grade.
    pub local: bool,
}

/// Parse a trailing size like `qwen3:14b`, `llama-70b`, `phi-3.8b`.
fn parse_size_billions(name: &str) -> Option<f64> {
    let tail = name.rsplit([':', '-', '_']).next().unwrap_or(name);
    let digits = tail.strip_suffix('b')?;
    digits
        .parse::<f64>()
        .ok()
        .filter(|v| *v > 0.0 && *v < 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::embedded(&Ladder::default()).unwrap()
    }

    fn grade(id: &str) -> usize {
        catalog().lookup(id).unwrap().grade_rank
    }

    #[test]
    fn entries_outside_ladder_are_skipped_not_fatal() {
        // a trimmed ladder must degrade the catalog, not break reloads
        let short = Ladder::new(["A+", "A", "B"].iter().map(|s| s.to_string()).collect());
        let c = Catalog::embedded(&short).expect("embedded catalog must parse");
        assert!(c.entries.iter().all(|e| e.grade_rank < 3));
        let full = catalog();
        assert!(
            c.entries.len() < full.entries.len(),
            "C-grade entries dropped: {} < {}",
            c.entries.len(),
            full.entries.len()
        );
    }

    #[test]
    fn embedded_catalog_parses() {
        let c = catalog();
        assert_eq!(c.version, 1);
        assert!(!c.entries.is_empty());
    }

    #[test]
    fn catalog_matches_full_id_and_last_segment() {
        let l = Ladder::default();
        assert_eq!(grade("zai/glm-5.3"), l.rank("A+").unwrap());
        assert_eq!(grade("or/z-ai/glm-5.3"), l.rank("A+").unwrap());
        assert_eq!(grade("or/anthropic/claude-opus-4.5"), l.rank("A+").unwrap());
        assert_eq!(grade("zai/glm-5.2"), l.rank("A").unwrap());
        // A+ specific beats generic: glm-5.3 is A+ even though glm* would be A
        assert_ne!(grade("zai/glm-5.3"), grade("zai/glm-4.6"));
        assert_eq!(grade("ollama/qwen3:14b"), l.rank("B").unwrap());
        assert_eq!(grade("ollama/llama3.1:8b"), l.rank("B").unwrap());
        assert_eq!(grade("ollama/llama3.2:1b"), l.rank("C").unwrap());
        assert_eq!(grade("p/gpt-5-mini"), l.rank("B").unwrap());
    }

    #[test]
    fn unknown_remote_quality_does_not_depend_on_price_name_or_context() {
        let l = Ladder::default();
        let c = catalog();
        for price in [None, Some((0.0, 0.0)), Some((0.1, 0.1)), Some((6.0, 6.0))] {
            for id in [
                "x/mystery",
                "x/mystery-ultra",
                "x/mystery-flash",
                "x/mystery:70b",
            ] {
                let g = c.heuristic_grade(
                    &l,
                    &HeuristicInput {
                        id,
                        price,
                        context: Some(1_000_000),
                        local: false,
                    },
                );
                assert_eq!(g.grade_rank, l.worst(), "{id}, {price:?}");
                assert_eq!(g.provenance, Provenance::Unknown);
            }
        }
    }

    #[test]
    fn unknown_local_models_never_infer_premium_quality() {
        let l = Ladder::default();
        let c = catalog();
        for (id, grade) in [("local/frontier-ultra:70b", "B"), ("local/tiny:1b", "C")] {
            let g = c.heuristic_grade(
                &l,
                &HeuristicInput {
                    id,
                    price: Some((0.1, 0.1)),
                    context: Some(1_000_000),
                    local: true,
                },
            );
            assert_eq!(l.token(g.grade_rank), grade);
        }
    }

    #[test]
    fn heuristic_no_price_and_local() {
        let l = Ladder::default();
        let c = catalog();
        // no data, not local → C + unknown, never guessed upward
        let x = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/mystery",
                price: None,
                context: None,
                local: false,
            },
        );
        assert_eq!(x.grade_rank, l.rank("C").unwrap());
        assert_eq!(x.provenance, Provenance::Unknown);
        // local → keyword/size rules from a B base
        let x = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/huge-local:70b",
                price: None,
                context: None,
                local: true,
            },
        );
        assert_eq!(x.grade_rank, l.rank("B").unwrap());
        assert!(x.rationale.contains("local"));
    }

    #[test]
    fn size_parser() {
        assert_eq!(parse_size_billions("qwen3:14b"), Some(14.0));
        assert_eq!(parse_size_billions("llama-70b"), Some(70.0));
        assert_eq!(parse_size_billions("phi-3.8b"), Some(3.8));
        assert_eq!(parse_size_billions("gpt-5.2"), None);
        assert_eq!(parse_size_billions("x:0"), None);
    }

    #[test]
    fn catalog_lookup_uses_full_id_patterns() {
        let l = Ladder::default();
        let c = Catalog::parse(
            r#"
            catalog_version = 1
            [[entry]]
            grade = "A"
            match = ["priv/*"]
        "#,
            &l,
        )
        .unwrap();
        assert!(c.lookup("priv/anything").is_some());
        assert!(c.lookup("other/priv").is_none());
    }
}
