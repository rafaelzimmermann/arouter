//! Auto-grading: embedded catalog (`src/catalog.toml`) + price/keyword/
//! size heuristic for unknown models (docs/grading.md). Manual
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
    #[serde(default)]
    heuristic: RawHeuristic,
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

#[derive(Debug, Deserialize, Default)]
struct RawHeuristic {
    a_plus_max: Option<f64>,
    a_max: Option<f64>,
    b_max: Option<f64>,
}

/// A graded catalog entry after parsing.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub grade_rank: usize,
    pub patterns: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct HeuristicBreakpoints {
    pub a_plus_max: f64,
    pub a_max: f64,
    pub b_max: f64,
}

impl Default for HeuristicBreakpoints {
    fn default() -> Self {
        Self {
            a_plus_max: 0.15,
            a_max: 1.0,
            b_max: 4.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub version: u32,
    /// Ordered: first matching entry wins.
    pub entries: Vec<CatalogEntry>,
    pub heuristic: HeuristicBreakpoints,
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
        let h = &raw.heuristic;
        Ok(Self {
            version: raw.catalog_version,
            entries,
            heuristic: HeuristicBreakpoints {
                a_plus_max: h.a_plus_max.unwrap_or(0.15),
                a_max: h.a_max.unwrap_or(1.0),
                b_max: h.b_max.unwrap_or(4.0),
            },
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

    /// Price/keyword/size heuristic with auditable rationale
    /// (docs/grading.md §Heuristic).
    pub fn heuristic_grade(&self, ladder: &Ladder, input: &HeuristicInput) -> AutoGrade {
        let name = input
            .id
            .rsplit('/')
            .next()
            .unwrap_or(input.id)
            .to_lowercase();
        let mut reasons: Vec<String> = Vec::new();
        let mut rank: usize;

        // 1. base grade from blended price — cheaper is favored: a priced
        // model earns its band when blended ≤ the band's ceiling. Free
        // (price 0) and unpriced models never guess upward (C + unknown).
        let blended = input.price.map(|(cin, cout)| (cin + 3.0 * cout) / 4.0);
        match blended {
            Some(b) if b <= 0.0 => {
                return AutoGrade {
                    grade_rank: ladder.rank("C").unwrap_or_else(|| ladder.worst()),
                    provenance: Provenance::Unknown,
                    rationale: "free (no price signal)".to_string(),
                };
            }
            Some(b) => {
                let hp = &self.heuristic;
                rank = if b <= hp.a_plus_max {
                    ladder.rank("A+").unwrap_or(0)
                } else if b <= hp.a_max {
                    ladder.rank("A").unwrap_or(1)
                } else if b <= hp.b_max {
                    ladder.rank("B").unwrap_or(2)
                } else {
                    ladder.rank("C").unwrap_or_else(|| ladder.worst())
                };
                reasons.push(format!("price ${b:.2}/Mtok"));
            }
            None if input.local => {
                // local engines skip pricing; keyword/size rules only
                rank = ladder.rank("B").unwrap_or(2);
                reasons.push("local (no price)".to_string());
            }
            None => {
                // never guess upward without data
                return AutoGrade {
                    grade_rank: ladder.rank("C").unwrap_or_else(|| ladder.worst()),
                    provenance: Provenance::Unknown,
                    rationale: "no price data".to_string(),
                };
            }
        }

        let a_rank = ladder.rank("A").unwrap_or(1);
        let b_rank = ladder.rank("B").unwrap_or(2);
        let cap = |rank: &mut usize, limit: usize, why: &str, reasons: &mut Vec<String>| {
            if *rank < limit {
                *rank = limit;
                reasons.push(why.to_string());
            }
        };

        // 2. keyword caps / floors
        let fast_kw = [
            "flash", "mini", "nano", "haiku", "lite", "small", "fast", "micro", "xs",
        ];
        let big_kw = ["opus", "ultra", "max", "frontier"];
        if fast_kw.iter().any(|k| name.contains(k)) {
            cap(&mut rank, b_rank, "keyword cap B", &mut reasons);
        }
        if big_kw.iter().any(|k| name.contains(k)) && rank > a_rank {
            rank = a_rank;
            reasons.push("keyword floor A".to_string());
        }

        // 3. size suffix (":27b", "-70b", "3.8b" at end of name)
        if let Some(bn) = parse_size_billions(&name) {
            if bn <= 9.0 {
                cap(
                    &mut rank,
                    b_rank,
                    &format!("size {bn:.1}b cap B"),
                    &mut reasons,
                );
            } else if bn <= 32.0 {
                cap(
                    &mut rank,
                    a_rank,
                    &format!("size {bn:.1}b cap A"),
                    &mut reasons,
                );
            }
        }

        // 4. very large context: one notch up
        if input.context.is_some_and(|c| c >= 262_144) && rank > 0 {
            rank -= 1;
            reasons.push("context ≥ 256k".to_string());
        }

        AutoGrade {
            grade_rank: rank,
            provenance: Provenance::Heuristic,
            rationale: reasons.join(", "),
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
    /// Local engines have no price by design — keyword/size rules only.
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
        // heuristic breakpoints are present and monotonic
        let h = &c.heuristic;
        assert!(h.a_plus_max < h.a_max && h.a_max < h.b_max);
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
    fn heuristic_prices() {
        let l = Ladder::default();
        let c = catalog();
        let h = |cin, cout| HeuristicInput {
            id: "x/mystery",
            price: Some((cin, cout)),
            context: None,
            local: false,
        };
        // cheaper is favored: low blended → high grade
        let cheap = c.heuristic_grade(&l, &h(0.1, 0.1)); // blended 0.1 ≤ a_plus_max
        assert_eq!(cheap.grade_rank, l.rank("A+").unwrap());
        assert!(cheap.rationale.contains("price $0.10"));
        let mid = c.heuristic_grade(&l, &h(1.0, 1.0)); // blended 1.0 ≤ a_max
        assert_eq!(mid.grade_rank, l.rank("A").unwrap());
        let premium = c.heuristic_grade(&l, &h(2.0, 2.0)); // blended 2.0 ≤ b_max
        assert_eq!(premium.grade_rank, l.rank("B").unwrap());
        let expensive = c.heuristic_grade(&l, &h(6.0, 6.0)); // blended 6.0 > b_max
        assert_eq!(expensive.grade_rank, l.rank("C").unwrap());
    }

    #[test]
    fn heuristic_free_and_unpriced_never_guessed_upward() {
        let l = Ladder::default();
        let c = catalog();
        // free (price 0) is not "cheap" — never guessed upward, even with
        // a frontier keyword that would otherwise floor it at A
        let free = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/mystery-ultra",
                price: Some((0.0, 0.0)),
                context: None,
                local: false,
            },
        );
        assert_eq!(free.grade_rank, l.rank("C").unwrap());
        assert_eq!(free.provenance, Provenance::Unknown);
    }

    #[test]
    fn heuristic_keyword_and_size_rules() {
        let l = Ladder::default();
        let c = catalog();
        // cheap but "flash" → capped at B
        let x = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/mystery-flash-pro",
                price: Some((0.1, 0.1)),
                context: None,
                local: false,
            },
        );
        assert_eq!(x.grade_rank, l.rank("B").unwrap());
        assert!(x.rationale.contains("keyword cap B"));
        // "micro" is also a tiny-model signal → capped at B even if cheap
        let x = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/nova-micro",
                price: Some((0.05, 0.05)),
                context: None,
                local: false,
            },
        );
        assert_eq!(x.grade_rank, l.rank("B").unwrap());
        assert!(x.rationale.contains("keyword cap B"));
        // small size caps even for cheap models
        let x = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/mystery:8b",
                price: Some((0.1, 0.1)),
                context: None,
                local: false,
            },
        );
        assert_eq!(x.grade_rank, l.rank("B").unwrap());
        assert!(x.rationale.contains("size 8.0b cap B"));
        // frontier keyword floors even an expensive unknown to A
        let x = c.heuristic_grade(
            &l,
            &HeuristicInput {
                id: "x/bargain-opus",
                price: Some((8.0, 8.0)),
                context: None,
                local: false,
            },
        );
        assert_eq!(x.grade_rank, l.rank("A").unwrap());
        assert!(x.rationale.contains("keyword floor A"));
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
