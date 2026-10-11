//! Grade ladder and request-grade parsing.
//!
//! Grades are ordered best→worst by position in the ladder (`A+`, `A`, `B`,
//! `C` by default; configurable). Requests carry either a single grade
//! (`"A+"`) or a ceiling..floor range (`"A+..A"`) per docs/routing.md §2.

/// Ordered grade ladder; index 0 is the best grade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ladder {
    pub grades: Vec<String>,
}

impl Default for Ladder {
    fn default() -> Self {
        Self::new(
            ["A+", "A", "B", "C"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }
}

/// A parsed request grade: ceiling plus optional floor (rank indices).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradeRequest {
    pub ceiling: usize,
    pub floor: Option<usize>,
}

pub const RANGE_SEP: &str = "..";

impl Ladder {
    pub fn new(grades: Vec<String>) -> Self {
        Self { grades }
    }

    pub fn rank(&self, token: &str) -> Option<usize> {
        self.grades.iter().position(|g| g == token)
    }

    /// Index of the worst (last) grade — the safe fallback when a
    /// heuristic wants a grade the ladder doesn't have.
    pub fn worst(&self) -> usize {
        self.grades.len().saturating_sub(1)
    }

    pub fn token(&self, rank: usize) -> &str {
        self.grades.get(rank).map(|s| s.as_str()).unwrap_or("?")
    }

    /// Parse a request model id: `"A+"` → ceiling A+; `"A+..A"` → ceiling A+,
    /// floor A. Returns `None` for unknown grades, inverted or empty ranges.
    pub fn parse_request(&self, raw: &str) -> Option<GradeRequest> {
        let raw = raw.trim();
        let (ceil_tok, floor_tok) = match raw.split_once(RANGE_SEP) {
            Some((c, f)) => (c, Some(f)),
            None => (raw, None),
        };
        if ceil_tok.is_empty() {
            return None;
        }
        let ceiling = self.rank(ceil_tok)?;
        let floor = match floor_tok.filter(|f| !f.is_empty()) {
            None => None, // no floor, or "A+.." treated as "A+"
            Some(f) => {
                let r = self.rank(f)?;
                // floor must be same grade or worse (higher rank index)
                if r < ceiling {
                    return None;
                }
                Some(r)
            }
        };
        Some(GradeRequest { ceiling, floor })
    }

    /// Ranks allowed by the fallback policy for a requested ceiling:
    /// the configured allow list for that grade, defaulting to
    /// `[itself, one grade lower]` (docs/routing.md §2). Sorted ascending
    /// (best first), always includes the ceiling.
    pub fn allow_ranks(&self, ceiling: usize, allow: Option<&Vec<String>>) -> Vec<usize> {
        let mut ranks: Vec<usize> = match allow {
            Some(tokens) if !tokens.is_empty() => {
                tokens.iter().filter_map(|t| self.rank(t)).collect()
            }
            // default policy: [itself, one grade lower] (docs/routing.md §2)
            _ => {
                let mut v = vec![ceiling];
                if ceiling + 1 < self.grades.len() {
                    v.push(ceiling + 1);
                }
                v
            }
        };
        if !ranks.contains(&ceiling) {
            ranks.push(ceiling);
        }
        ranks.sort_unstable();
        ranks.dedup();
        ranks
    }

    /// Intersect the allow set with an explicit request floor: a request for
    /// `A+..A` never drops below `A`, even if the ladder allows `B`.
    pub fn effective_ranks(
        &self,
        ceiling: usize,
        allow: Option<&Vec<String>>,
        floor: Option<usize>,
    ) -> Vec<usize> {
        let allowed = self.allow_ranks(ceiling, allow);
        match floor {
            Some(f) => allowed.into_iter().filter(|&r| r <= f).collect(),
            None => allowed,
        }
    }

    pub fn list(&self) -> String {
        self.grades.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_grades() {
        let l = Ladder::default();
        assert_eq!(
            l.parse_request("A+"),
            Some(GradeRequest {
                ceiling: 0,
                floor: None
            })
        );
        assert_eq!(
            l.parse_request("A"),
            Some(GradeRequest {
                ceiling: 1,
                floor: None
            })
        );
        assert_eq!(
            l.parse_request(" B "),
            Some(GradeRequest {
                ceiling: 2,
                floor: None
            })
        );
    }

    #[test]
    fn parse_ranges() {
        let l = Ladder::default();
        assert_eq!(
            l.parse_request("A+..A"),
            Some(GradeRequest {
                ceiling: 0,
                floor: Some(1)
            })
        );
        assert_eq!(
            l.parse_request("A..C"),
            Some(GradeRequest {
                ceiling: 1,
                floor: Some(3)
            })
        );
        // "A+.." == "A+"
        assert_eq!(
            l.parse_request("A+.."),
            Some(GradeRequest {
                ceiling: 0,
                floor: None
            })
        );
        // same grade range is valid (strict)
        assert_eq!(
            l.parse_request("A..A"),
            Some(GradeRequest {
                ceiling: 1,
                floor: Some(1)
            })
        );
    }

    #[test]
    fn parse_rejects_unknown_and_inverted() {
        let l = Ladder::default();
        assert_eq!(l.parse_request("F"), None);
        assert_eq!(l.parse_request(""), None);
        assert_eq!(l.parse_request(".."), None);
        // floor better than ceiling is nonsense
        assert_eq!(l.parse_request("A..A+"), None);
        assert_eq!(l.parse_request("B..A+"), None);
    }

    #[test]
    fn allow_defaults_to_one_step_drop() {
        let l = Ladder::default();
        assert_eq!(l.allow_ranks(0, None), vec![0, 1]); // A+ → A+ or A
        assert_eq!(l.allow_ranks(1, None), vec![1, 2]); // A → A or B
                                                        // worst grade: no lower exists
        assert_eq!(l.allow_ranks(3, None), vec![3]);
    }

    #[test]
    fn allow_config_overrides() {
        let l = Ladder::default();
        let strict = vec!["A+".to_string()];
        assert_eq!(l.allow_ranks(0, Some(&strict)), vec![0]);
        let deep = vec!["A+".to_string(), "A".to_string(), "B".to_string()];
        assert_eq!(l.allow_ranks(0, Some(&deep)), vec![0, 1, 2]);
        // ceiling always included even if the user forgot it
        let noceil = vec!["A".to_string()];
        assert_eq!(l.allow_ranks(0, Some(&noceil)), vec![0, 1]);
        // unknown tokens ignored
        let junk = vec!["X".to_string()];
        assert_eq!(l.allow_ranks(0, Some(&junk)), vec![0]);
    }

    #[test]
    fn effective_intersects_request_floor() {
        let l = Ladder::default();
        let deep = vec!["A+".to_string(), "A".to_string(), "B".to_string()];
        // request A+..A caps at A even though ladder allows B
        assert_eq!(l.effective_ranks(0, Some(&deep), Some(1)), vec![0, 1]);
        // request without floor uses full allow list
        assert_eq!(l.effective_ranks(0, Some(&deep), None), vec![0, 1, 2]);
        // strict request wins over deeper default
        assert_eq!(l.effective_ranks(0, None, Some(0)), vec![0]);
    }

    #[test]
    fn custom_ladder() {
        let l = Ladder::new(vec!["S".into(), "A".into(), "B".into()]);
        assert_eq!(l.parse_request("S..B").unwrap().ceiling, 0);
        assert_eq!(l.token(2), "B");
    }
}
