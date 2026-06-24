//! Case-insensitive substring matching within a terminal line (a slice of cell
//! characters). Pure + unit-tested; the element highlights matches and the
//! session scans the scrollback with these.

/// Non-overlapping match column ranges `(start, len)` of `needle` in `haystack`
/// (case-insensitive, ASCII).
pub fn match_ranges(haystack: &[char], needle: &[char]) -> Vec<(usize, usize)> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if haystack[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| h.eq_ignore_ascii_case(n))
        {
            out.push((i, needle.len()));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Whether `haystack` contains `needle` (case-insensitive).
pub fn line_contains(haystack: &[char], needle: &[char]) -> bool {
    needle.len() <= haystack.len()
        && !needle.is_empty()
        && (0..=haystack.len() - needle.len()).any(|i| {
            haystack[i..i + needle.len()]
                .iter()
                .zip(needle)
                .all(|(h, n)| h.eq_ignore_ascii_case(n))
        })
}

#[cfg(test)]
mod tests {
    use super::{line_contains, match_ranges};
    fn c(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn finds_all_case_insensitive() {
        assert_eq!(
            match_ranges(&c("Foo foo FOO"), &c("foo")),
            vec![(0, 3), (4, 3), (8, 3)]
        );
    }

    #[test]
    fn no_match() {
        assert!(match_ranges(&c("hello"), &c("xyz")).is_empty());
        assert!(!line_contains(&c("hello"), &c("xyz")));
    }

    #[test]
    fn contains_is_case_insensitive() {
        assert!(line_contains(&c("a Needle here"), &c("needle")));
    }

    #[test]
    fn empty_or_oversized_needle() {
        assert!(match_ranges(&c("abc"), &c("")).is_empty());
        assert!(!line_contains(&c("abc"), &c("")));
        assert!(!line_contains(&c("ab"), &c("abc")));
    }
}
