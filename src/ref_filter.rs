//! Ref patterns that a host or a repository leaves out of its fetches.
//!
//! A pattern is a Git ref name with at most one `*`, for example
//! `refs/pull/*`. The fetch gets it as a negative refspec (`^refs/pull/*`).

use crate::git::mirror::ARCHIVE_REF_PREFIX;

/// More patterns than this are not a real use and make the config large.
pub const MAX_PATTERNS: usize = 50;
const MAX_PATTERN_LEN: usize = 200;

/// A pattern that Git cannot use or that this archive does not allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPattern(pub String);

/// Checks the patterns of a form field, one per line, and returns them in
/// the stored form: one pattern per line, without blank lines and repeats.
/// A field without patterns gives `None`.
pub fn normalize(text: &str) -> Result<Option<String>, InvalidPattern> {
    let mut patterns: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !is_valid(line) {
            return Err(InvalidPattern(line.to_owned()));
        }
        if !patterns.contains(&line) {
            patterns.push(line);
        }
    }
    if patterns.len() > MAX_PATTERNS {
        return Err(InvalidPattern(patterns[MAX_PATTERNS].to_owned()));
    }
    Ok((!patterns.is_empty()).then(|| patterns.join("\n")))
}

/// The stored patterns of a host and of a repository, without repeats.
pub fn combine(host: Option<&str>, repo: Option<&str>) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for pattern in [host, repo].into_iter().flatten().flat_map(str::lines) {
        let pattern = pattern.trim();
        if !pattern.is_empty() && !all.iter().any(|p| p == pattern) {
            all.push(pattern.to_owned());
        }
    }
    all
}

/// Whether the ref `name` matches `pattern` as Git matches a refspec: the
/// one `*` stands for any text, `/` included.
pub fn matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail)
        }
    }
}

/// Whether any of `patterns` matches `name`.
pub fn is_excluded(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|pattern| matches(pattern, name))
}

/// Parts that Git does not allow inside a ref name.
const FORBIDDEN_PARTS: &[&str] = &["..", "//", "@{"];
/// Endings that Git does not allow for a ref name.
const FORBIDDEN_ENDINGS: &[&str] = &["/", ".", ".lock"];

fn is_valid(pattern: &str) -> bool {
    let Some(rest) = pattern.strip_prefix("refs/") else {
        return false;
    };
    let checks = [
        !rest.is_empty(),
        pattern.len() <= MAX_PATTERN_LEN,
        !pattern.starts_with(ARCHIVE_REF_PREFIX),
        pattern.matches('*').count() <= 1,
        !FORBIDDEN_PARTS.iter().any(|part| pattern.contains(part)),
        !FORBIDDEN_ENDINGS.iter().any(|end| pattern.ends_with(end)),
        pattern.split('/').all(is_allowed_component),
        pattern.chars().all(is_allowed_char),
    ];
    checks.iter().all(|ok| *ok)
}

/// Git refuses a ref whose `/`-separated part starts with `.` or ends with
/// `.lock`, anywhere in the name.
fn is_allowed_component(part: &str) -> bool {
    !part.starts_with('.') && !part.ends_with(".lock")
}

/// Git forbids these in ref names; `*` is the one wildcard a refspec takes.
fn is_allowed_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !matches!(c, '^' | ':' | '?' | '[' | '\\' | '~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_valid_patterns_once() {
        assert_eq!(normalize("").unwrap(), None);
        assert_eq!(normalize(" \n\n ").unwrap(), None);
        assert_eq!(
            normalize("refs/pull/*\r\n  refs/merge-requests/*/head \nrefs/pull/*\n").unwrap(),
            Some("refs/pull/*\nrefs/merge-requests/*/head".to_owned())
        );
    }

    #[test]
    fn normalize_rejects_patterns_git_or_the_archive_cannot_take() {
        for bad in [
            "pull/*",
            "refs/",
            "refs/*/*",
            "refs/pull/ *",
            "^refs/pull/*",
            "refs/pull:refs/x",
            "refs/a..b",
            "refs/kehribar/rewritten/*",
            "refs/heads/x.lock",
            "refs/heads/x/",
            "refs/heads/a?",
            // Inside the name too: git fails the whole fetch on these.
            "refs/heads/.x",
            "refs/heads/a/.b/*",
            "refs/heads/a.lock/b",
        ] {
            assert_eq!(
                normalize(bad),
                Err(InvalidPattern(bad.trim().to_owned())),
                "{bad}"
            );
        }
    }

    #[test]
    fn patterns_match_like_git_refspecs() {
        assert!(matches("refs/pull/*", "refs/pull/12/head"));
        assert!(matches(
            "refs/merge-requests/*/head",
            "refs/merge-requests/3/head"
        ));
        assert!(!matches(
            "refs/merge-requests/*/head",
            "refs/merge-requests/3/merge"
        ));
        assert!(matches("refs/heads/tmp", "refs/heads/tmp"));
        assert!(!matches("refs/heads/tmp", "refs/heads/tmp2"));
        // The head and the tail of a pattern never overlap.
        assert!(!matches("refs/a*a", "refs/a"));
        let patterns = ["refs/pull/*".to_owned(), "refs/heads/wip-*".to_owned()];
        assert!(is_excluded(&patterns, "refs/heads/wip-x"));
        assert!(!is_excluded(&patterns, "refs/heads/main"));
    }

    #[test]
    fn too_many_patterns_are_rejected() {
        let text: String = (0..=MAX_PATTERNS)
            .map(|i| format!("refs/x/{i}\n"))
            .collect();
        assert!(normalize(&text).is_err());
    }

    #[test]
    fn combine_merges_host_and_repository_patterns() {
        assert!(combine(None, None).is_empty());
        assert_eq!(
            combine(
                Some("refs/pull/*\nrefs/keep/*"),
                Some("refs/pull/*\nrefs/x")
            ),
            ["refs/pull/*", "refs/keep/*", "refs/x"]
        );
    }
}
