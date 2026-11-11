//! Repository name patterns of a watched source.
//!
//! A pattern is a glob on the repository name (the last path segment):
//! `*` matches any run of characters, `?` one character, case does not
//! matter. A name passes when it matches one include pattern, or when there
//! is none, and matches no exclude pattern.

use crate::repo_url::is_name_byte;

/// More patterns than this are not a real use.
pub const MAX_PATTERNS: usize = 50;
const MAX_PATTERN_LEN: usize = 100;

/// A pattern with a character a repository name cannot hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPattern(pub String);

/// Checks the patterns of a form field, one per line, and returns them in
/// the stored form: one per line, without blank lines and repeats. A field
/// without patterns gives `None`.
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

fn is_valid(pattern: &str) -> bool {
    pattern.len() <= MAX_PATTERN_LEN
        && pattern
            .bytes()
            .all(|b| is_name_byte(b) || matches!(b, b'*' | b'?'))
}

/// The stored include and exclude patterns of a source.
#[derive(Debug, Clone, Copy)]
pub struct NameFilter<'a> {
    pub include: Option<&'a str>,
    pub exclude: Option<&'a str>,
}

impl NameFilter<'_> {
    pub fn allows(&self, name: &str) -> bool {
        let any = |patterns: Option<&str>| {
            patterns
                .into_iter()
                .flat_map(str::lines)
                .any(|pattern| matches(pattern.trim(), name))
        };
        let included = self.include.is_none_or(|_| any(self.include));
        included && !any(self.exclude)
    }
}

/// Whether `name` matches the glob `pattern`, ignoring ASCII case.
pub fn matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<u8> = pattern.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let name: Vec<u8> = name.bytes().map(|b| b.to_ascii_lowercase()).collect();
    glob(&pattern, &name)
}

/// Iterative glob match with one backtrack point for the last `*`.
fn glob(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == b'?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                // Let the last `*` take one more character and try again.
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&b| b == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_whole_names_without_case() {
        let cases = [
            ("api-*", "api-server", true),
            ("api-*", "API-Server", true),
            ("api-*", "my-api-server", false),
            ("*-old", "tool-old", true),
            ("*-old", "tool-older", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("*", "anything", true),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("exact", "exact", true),
            ("exact", "exactly", false),
        ];
        for (pattern, name, expected) in cases {
            assert_eq!(matches(pattern, name), expected, "{pattern} vs {name}");
        }
    }

    #[test]
    fn exclude_wins_and_no_include_means_all() {
        let all = NameFilter {
            include: None,
            exclude: None,
        };
        assert!(all.allows("app"));
        let filter = NameFilter {
            include: Some("api-*\nweb"),
            exclude: Some("*-old"),
        };
        assert!(filter.allows("api-core"));
        assert!(filter.allows("web"));
        assert!(!filter.allows("docs"));
        assert!(!filter.allows("api-old"));
        let only_exclude = NameFilter {
            include: None,
            exclude: Some("tmp-*"),
        };
        assert!(only_exclude.allows("app") && !only_exclude.allows("tmp-1"));
    }

    #[test]
    fn patterns_are_checked_and_stored_one_per_line() {
        assert_eq!(normalize("  \n"), Ok(None));
        assert_eq!(
            normalize(" api-*\n\n*-old\napi-*\n"),
            Ok(Some("api-*\n*-old".to_owned()))
        );
        for bad in ["a/b", "a b", "<x>", "a\\b"] {
            assert_eq!(normalize(bad), Err(InvalidPattern(bad.to_owned())), "{bad}");
        }
        let many: String = (0..=MAX_PATTERNS).map(|i| format!("p{i}\n")).collect();
        assert!(normalize(&many).is_err());
    }
}
