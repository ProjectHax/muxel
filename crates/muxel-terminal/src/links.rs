//! Detecting clickable URLs and file paths within a terminal line (a slice of
//! cell characters).
//!
//! Pure + unit-tested so the element can stay simple: it reads a line's chars
//! from the grid and asks whether the clicked column lands on a URL or a path.
//! Path detection only produces *candidates* — the element resolves them against
//! the pane's cwd and checks existence before treating one as clickable.

use std::path::{Path, PathBuf};

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

/// The URL covering column `col`, with its `(start, end)` span, if any.
pub fn url_span_at(line: &[char], col: usize) -> Option<(usize, usize, String)> {
    url_spans(line)
        .into_iter()
        .find(|(s, e)| col >= *s && col < *e)
        .map(|(s, e)| (s, e, line[s..e].iter().collect()))
}

/// Characters that may appear inside a file path. `:` is included so a trailing
/// `:line[:col]` suffix stays inside the visual span (it's stripped from the
/// returned path string). `\` is accepted so Windows paths (`D:\dev\foo.rs`) are
/// candidates the same way as POSIX paths.
fn is_path_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '/' | '\\' | '.' | '_' | '-' | '~' | '+' | '@' | '%' | '#' | ':'
        )
}

/// Strip a trailing `:line[:col]` suffix (e.g. `src/x.rs:42:7` → `src/x.rs`).
fn strip_line_suffix(token: &[char]) -> &[char] {
    let mut end = token.len();
    for _ in 0..2 {
        let digits_start = token[..end]
            .iter()
            .rposition(|c| !c.is_ascii_digit())
            .map(|i| i + 1)
            .unwrap_or(0);
        if digits_start < end && digits_start > 0 && token[digits_start - 1] == ':' {
            end = digits_start - 1;
        } else {
            break;
        }
    }
    &token[..end]
}

/// All file-path *candidate* spans `(start, end, path)` in `line`. The span
/// covers the full token (including any `:line:col` suffix, so an underline
/// covers what the user sees); `path` has the suffix stripped. Candidates are
/// syntactic only — callers must resolve + existence-check them.
pub fn path_spans(line: &[char]) -> Vec<(usize, usize, String)> {
    let mut spans = Vec::new();
    let n = line.len();
    let mut i = 0;
    while i < n {
        if !is_path_char(line[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let mut j = i;
        while j < n && is_path_char(line[j]) {
            j += 1;
        }
        i = j;
        // Trim trailing sentence punctuation off the token.
        while j > start && is_trailing_punct(line[j - 1]) {
            j -= 1;
        }
        let token = &line[start..j];
        // Must look like a path: contains '/' or '\', isn't a URL (those have
        // "://"), and starts with a plausible path lead-in (incl. `D:` drives).
        let has_slash = token.contains(&'/') || token.contains(&'\\');
        let is_url = token.windows(3).any(|w| w == [':', '/', '/']);
        let good_start = token.first().is_some_and(|c| {
            *c == '/'
                || *c == '\\'
                || *c == '~'
                || *c == '.'
                || c.is_alphanumeric()
                || *c == '_'
        });
        if has_slash && !is_url && good_start && token.len() >= 2 {
            let path: String = strip_line_suffix(token).iter().collect();
            if !path.is_empty() && path != "/" && path != "\\" {
                spans.push((start, j, path));
            }
        }
    }
    spans
}

/// The file-path candidate covering column `col`, if any.
pub fn path_span_at(line: &[char], col: usize) -> Option<(usize, usize, String)> {
    path_spans(line)
        .into_iter()
        .find(|(s, e, _)| col >= *s && col < *e)
}

/// Resolve a raw path token against the pane's `cwd` (for relative paths) and
/// `home` (for `~`). `None` when the needed base is unavailable — e.g. a remote
/// pane with no local cwd.
pub fn resolve_path(raw: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if raw == "~" {
        return home.map(Path::to_path_buf);
    }
    if raw.starts_with('/') {
        return Some(PathBuf::from(raw));
    }
    cwd.map(|c| c.join(raw))
}

/// A `file://` URI for an absolute path, percent-encoding everything outside
/// the unreserved set + `/` (so spaces etc. survive the trip through xdg-open).
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(*b as char);
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::{file_uri, path_span_at, path_spans, resolve_path, url_span_at, url_spans};
    use std::path::{Path, PathBuf};

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// Test shorthand: the URL string covering `col`.
    fn find_url_at(line: &[char], col: usize) -> Option<String> {
        url_span_at(line, col).map(|(_, _, url)| url)
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

    // ---- file paths ----

    #[test]
    fn finds_absolute_path() {
        let line = chars("error in /usr/lib/foo.rs today");
        let (s, e, p) = path_span_at(&line, 12).unwrap();
        assert_eq!(p, "/usr/lib/foo.rs");
        assert_eq!(&line[s..e].iter().collect::<String>(), "/usr/lib/foo.rs");
    }

    #[test]
    fn finds_home_and_dot_relative_paths() {
        let line = chars("see ~/projects/x.txt and ./src/lib.rs and ../up.c");
        assert_eq!(path_spans(&line).len(), 3);
        assert_eq!(path_span_at(&line, 6).unwrap().2, "~/projects/x.txt");
        assert_eq!(path_span_at(&line, 27).unwrap().2, "./src/lib.rs");
        assert_eq!(path_span_at(&line, 44).unwrap().2, "../up.c");
    }

    #[test]
    fn finds_windows_path_with_backslashes() {
        let line = chars(r"open D:\dev\muxel\src\app.rs please");
        let p = path_span_at(&line, 10).expect("windows path candidate");
        assert_eq!(p.2, r"D:\dev\muxel\src\app.rs");
    }

    #[test]
    fn strips_line_col_suffix_but_spans_it() {
        let line = chars("at src/main.rs:42:7 in build");
        let (s, e, p) = path_span_at(&line, 5).unwrap();
        assert_eq!(p, "src/main.rs");
        // The visual span still covers the ":42:7" suffix.
        assert_eq!(&line[s..e].iter().collect::<String>(), "src/main.rs:42:7");
    }

    #[test]
    fn strips_trailing_punctuation_from_paths() {
        let line = chars("wrote src/main.rs.");
        assert_eq!(path_span_at(&line, 8).unwrap().2, "src/main.rs");
        let line = chars("(see /tmp/x/y,)");
        assert_eq!(path_span_at(&line, 6).unwrap().2, "/tmp/x/y");
    }

    #[test]
    fn urls_are_not_path_candidates() {
        let line = chars("go to https://example.com/x/y now");
        assert!(path_spans(&line).is_empty());
    }

    #[test]
    fn plain_words_are_not_paths() {
        let line = chars("compiling twelve deps for release");
        assert!(path_spans(&line).is_empty());
    }

    #[test]
    fn resolve_path_handles_home_relative_and_absolute() {
        let cwd = Path::new("/work/proj");
        let home = Path::new("/home/u");
        assert_eq!(
            resolve_path("src/x.rs", Some(cwd), Some(home)),
            Some(PathBuf::from("/work/proj/src/x.rs"))
        );
        assert_eq!(
            resolve_path("~/y.txt", Some(cwd), Some(home)),
            Some(PathBuf::from("/home/u/y.txt"))
        );
        assert_eq!(
            resolve_path("/abs/z", None, None),
            Some(PathBuf::from("/abs/z"))
        );
        // Relative with no cwd (e.g. a remote pane) → unresolvable.
        assert_eq!(resolve_path("src/x.rs", None, Some(home)), None);
        assert_eq!(resolve_path("~/y", Some(cwd), None), None);
    }

    #[test]
    fn file_uri_percent_encodes() {
        assert_eq!(
            file_uri(Path::new("/tmp/a b/c#d.rs")),
            "file:///tmp/a%20b/c%23d.rs"
        );
        assert_eq!(
            file_uri(Path::new("/plain/path.rs")),
            "file:///plain/path.rs"
        );
    }
}
