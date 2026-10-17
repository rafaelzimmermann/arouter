//! Tiny glob matching with `*` wildcards (case-sensitive), used for
//! auto-grading catalog patterns and profile visibility filters.

/// Match `text` against `pattern` where `*` matches any (possibly empty)
/// sequence of characters. No other metacharacters.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Two-pointer with backtracking on the last '*'.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == '?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Match against any of the patterns (empty pattern list = no match).
pub fn glob_any(patterns: &[String], text: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn exact_and_prefix_suffix() {
        assert!(glob_match("glm-5.3", "glm-5.3"));
        assert!(!glob_match("glm-5.3", "glm-5.2"));
        assert!(glob_match("glm-*", "glm-5.3"));
        assert!(glob_match("*-flash", "glm-5.3-flash"));
        assert!(!glob_match("*-flash", "glm-5.3"));
    }

    #[test]
    fn star_matches_empty_and_multiline_names() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything/with:colons"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXbYcZ"));
    }

    #[test]
    fn multiple_stars_backtrack() {
        assert!(glob_match("*flash*next*", "qwen3.8-flash-next-coder-iq1_m"));
        assert!(!glob_match("*flash*next*", "qwen3.8-coder"));
        assert!(glob_match(
            "openrouter/*opus*",
            "openrouter/anthropic/claude-opus-4.6"
        ));
    }

    #[test]
    fn question_mark_single_char() {
        assert!(glob_match("glm-?.3", "glm-5.3"));
        assert!(!glob_match("glm-?.3", "glm-5.33"));
    }

    #[test]
    fn glob_any_semantics() {
        assert!(glob_any(&s(&["*opus*", "*grok*"]), "x/claude-opus-4.6"));
        assert!(!glob_any(&s(&["*opus*"]), "x/glm-5.3"));
        assert!(!glob_any(&[], "anything"));
    }
}
