//! Detecting clickable URLs within a terminal line (a slice of cell characters).
//!
//! Pure + unit-tested so the element can stay simple: it reads a line's chars
//! from the grid and asks whether the clicked column lands on a URL.

/// Does the text starting at `i` begin an `http://` or `https://` scheme?
fn starts_scheme(line: &[char], i: usize) -> bool {
    const SCHEMES: [&[char]; 2] = [
        &['h', 't', 't', 'p', ':', '/', '/'],
        &['h', 't', 't', 'p', 's', ':', '/', '/'],
    ];
    SCHEMES.iter().any(|s| line[i..].starts_with(s))
}

/// Characters that can appear inside a URL (everything but whitespace and a few
/// delimiters that usually bracket a URL rather than belong to it).
fn is_url_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '\0'
        )
}

/// Trailing punctuation that's almost always sentence punctuation, not URL.
fn is_trailing_punct(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?')
}

/// All URL spans `(start, end)` (end exclusive, in column indices) in `line`.
pub fn url_spans(line: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let n = line.len();
    let mut i = 0;
    while i < n {
        if starts_scheme(line, i) {
            let mut j = i;
            while j < n && is_url_char(line[j]) {
                j += 1;
            }
            while j > i && is_trailing_punct(line[j - 1]) {
                j -= 1;
            }
            // A bare scheme ("https://") isn't a useful link.
            if line[i..j].iter().filter(|c| **c == '/').count() > 2 || j - i > 9 {
                spans.push((i, j));
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    spans
}

/// The URL covering column `col`, if any.
pub fn find_url_at(line: &[char], col: usize) -> Option<String> {
    url_spans(line)
        .into_iter()
        .find(|(s, e)| col >= *s && col < *e)
        .map(|(s, e)| line[s..e].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::{find_url_at, url_spans};

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn finds_url_under_column() {
        let line = chars("see https://example.com/x here");
        let url = find_url_at(&line, 10).unwrap();
        assert_eq!(url, "https://example.com/x");
    }

    #[test]
    fn none_outside_url() {
        let line = chars("see https://example.com here");
        assert_eq!(find_url_at(&line, 0), None); // on "see"
        assert_eq!(find_url_at(&line, 27), None); // on "here"
    }

    #[test]
    fn trims_trailing_punctuation() {
        let line = chars("visit https://a.example.com/p.");
        assert_eq!(
            find_url_at(&line, 8).as_deref(),
            Some("https://a.example.com/p")
        );
    }

    #[test]
    fn ignores_non_http() {
        let line = chars("run ftp://host/file or foo");
        assert!(url_spans(&line).is_empty());
    }

    #[test]
    fn two_urls_on_one_line() {
        let line = chars("https://a.com/1 and https://b.com/2");
        assert_eq!(url_spans(&line).len(), 2);
        assert_eq!(find_url_at(&line, 25).as_deref(), Some("https://b.com/2"));
    }
}
