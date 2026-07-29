//! Detecting clickable URLs and file paths within a terminal line (a slice of
//! cell characters).
//!
//! Pure + unit-tested so the element can stay simple: it reads a line's chars
//! from the grid and asks whether the clicked column lands on a URL or a path.
//! Path detection only produces *candidates* — the element resolves them against
//! the pane's cwd and checks existence before treating one as clickable.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLinkTarget {
    pub path: PathBuf,
    /// One-based source line from a `#L12` fragment.
    pub line: Option<u32>,
    /// One-based source column from a `#L12C4` fragment.
    pub column: Option<u32>,
}

/// A logical line reconstructed from terminal rows, plus the clicked row's
/// location inside it so a match can be painted back onto that row.
pub struct StitchedRows {
    pub chars: Vec<char>,
    pub clicked_col: usize,
    pub clicked_in_content: bool,
    pub row_base: usize,
    pub row_start: usize,
    pub row_len: usize,
}

/// Blank the chrome outside a TUI row's paired vertical box borders.
///
/// Grok renders Markdown inside `│ ... │`. Those border cells are real terminal
/// characters, so joining wrapped rows without removing them produces targets
/// such as `file:///│D:/path/│file.html`.
pub fn strip_box_margins(row: &mut [char]) {
    let is_vertical = |c: char| matches!(c, '│' | '┃' | '║');
    let Some(first) = row.iter().position(|c| is_vertical(*c)) else {
        return;
    };
    let Some(last) = row.iter().rposition(|c| is_vertical(*c)) else {
        return;
    };
    // Internal table/tree separators are content, not box chrome. A box must
    // have one border near each edge of the captured terminal row.
    if first == last || first > 4 || last.saturating_add(5) < row.len() {
        return;
    }
    row[..=first].fill(' ');
    row[last..].fill(' ');
}

/// Join rows selected as one visual token.
///
/// A terminal soft wrap preserves every column. A TUI-generated hard wrap has
/// already inserted a real row break, often with a right margin and hanging
/// indent, so its previous row is right-trimmed and its continuation left-trimmed.
pub fn stitch_rows(
    rows: &[(Vec<char>, bool)],
    clicked_row: usize,
    clicked_col: usize,
) -> StitchedRows {
    let mut chars = Vec::new();
    let mut row_base = 0;
    let mut row_start = 0;
    let mut row_len = 0;
    let mut clicked_in_content = false;
    for (index, (row, soft_wraps_to_next)) in rows.iter().enumerate() {
        let hard_continuation = index > 0 && !rows[index - 1].1;
        let start = if hard_continuation {
            row.iter()
                .position(|c| !c.is_whitespace())
                .unwrap_or(row.len())
        } else {
            0
        };
        let end = if *soft_wraps_to_next {
            row.len()
        } else {
            row.iter()
                .rposition(|c| !c.is_whitespace())
                .map(|column| column + 1)
                .unwrap_or(start)
        };
        if index == clicked_row {
            row_base = chars.len();
            row_start = start;
            row_len = end.saturating_sub(start);
            clicked_in_content = clicked_col >= start && clicked_col < end;
        }
        chars.extend(row[start..end].iter().copied());
    }
    StitchedRows {
        clicked_col: row_base + clicked_col.saturating_sub(row_start),
        clicked_in_content,
        chars,
        row_base,
        row_start,
        row_len,
    }
}

/// Whether a TUI row plausibly hard-wraps a link/path onto the next row.
/// Grok can render inside a narrower content column, so a `file:///` target and
/// its path continuation may end well before the terminal's right edge.
pub fn hard_wraps_to_next(row: &[char], next_text_column: usize) -> bool {
    let Some(end) = row
        .iter()
        .rposition(|c| !c.is_whitespace())
        .map(|column| column + 1)
    else {
        return false;
    };
    // TUI content boxes can be inset well beyond a dozen terminal columns.
    // A continuation still has to begin in the left half; URI/path parsing and
    // existence checks remain the final guards against joining ordinary rows.
    if next_text_column >= row.len() / 2 {
        return false;
    }
    let content = &row[..end];
    let url_at_end = url_spans(content)
        .into_iter()
        .find(|(_, span_end)| *span_end == end);
    let path_at_end = path_spans(content)
        .into_iter()
        .find(|(_, span_end, _)| *span_end == end);
    let near_grid_edge = end.saturating_add(16) >= row.len();
    let path_only = path_at_end
        .as_ref()
        .is_some_and(|(start, _, _)| content[..*start].iter().all(|c| c.is_whitespace()));
    let parenthesized_file_url = url_at_end.is_some_and(|(start, _)| {
        let url: String = content[start..].iter().collect();
        url.starts_with("file:///") && url.ends_with('/') && content[..start].contains(&'(')
    });
    path_at_end.is_some() && (near_grid_edge || path_only)
        || parenthesized_file_url
        || near_grid_edge && url_at_end.is_some()
}

/// Does the text starting at `i` begin a supported URI scheme?
fn starts_scheme(line: &[char], i: usize) -> bool {
    const SCHEMES: [&[char]; 3] = [
        &['h', 't', 't', 'p', ':', '/', '/'],
        &['h', 't', 't', 'p', 's', ':', '/', '/'],
        &['f', 'i', 'l', 'e', ':', '/', '/'],
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

/// A literal Markdown inline link whose label covers `col`.
///
/// Terminal applications usually convert Markdown links to OSC 8. This bounded
/// scanner covers applications that print `[label](target)` literally without
/// pulling a Markdown parser into the paint path.
pub fn markdown_link_at(line: &[char], col: usize) -> Option<(usize, usize, String)> {
    let mut i = 0;
    while i < line.len() {
        if line[i] != '[' {
            i += 1;
            continue;
        }
        let Some(label_end) = line[i + 1..]
            .iter()
            .position(|c| *c == ']')
            .map(|end| end + i + 1)
        else {
            i += 1;
            continue;
        };
        if line.get(label_end + 1) != Some(&'(') {
            i = label_end + 1;
            continue;
        }
        let target_start = label_end + 2;
        let Some(target_end) = line[target_start..]
            .iter()
            .position(|c| *c == ')')
            .map(|end| end + target_start)
        else {
            i = target_start;
            continue;
        };
        let valid_target = !line[target_start..target_end]
            .iter()
            .any(|c| c.is_whitespace());
        if col > i && col < label_end && target_end > target_start && valid_target {
            let target: String = line[target_start..target_end].iter().collect();
            return Some((i + 1, label_end, target));
        }
        i = target_end + 1;
    }
    None
}

/// A literal Markdown inline link whose destination covers `col`.
///
/// This complements [`markdown_link_at`]: TUIs can expose both the readable
/// label and the wrapped `(file:///...)` destination. Clicking either should
/// open the complete destination, not the row-local URI fragment.
pub fn markdown_target_at(line: &[char], col: usize) -> Option<(usize, usize, String)> {
    let mut i = 0;
    while i < line.len() {
        if line[i] != '[' {
            i += 1;
            continue;
        }
        let Some(label_end) = line[i + 1..]
            .iter()
            .position(|c| *c == ']')
            .map(|end| end + i + 1)
        else {
            i += 1;
            continue;
        };
        if line.get(label_end + 1) != Some(&'(') {
            i = label_end + 1;
            continue;
        }
        let target_start = label_end + 2;
        let Some(target_end) = line[target_start..]
            .iter()
            .position(|c| *c == ')')
            .map(|end| end + target_start)
        else {
            i = target_start;
            continue;
        };
        let valid_target = !line[target_start..target_end]
            .iter()
            .any(|c| c.is_whitespace());
        if col >= target_start && col < target_end && target_end > target_start && valid_target {
            let target: String = line[target_start..target_end].iter().collect();
            return Some((target_start, target_end, target));
        }
        i = target_end + 1;
    }
    None
}

/// A Markdown link after a TUI has rendered away the brackets but left its
/// destination visible: `label (file:///path)` or `label (https://host/path)`.
/// Grok uses this form without emitting an OSC 8 hyperlink for the label.
pub fn rendered_markdown_link_at(line: &[char], col: usize) -> Option<(usize, usize, String)> {
    for target_open in 0..line.len().saturating_sub(1) {
        if line[target_open] != '(' || !starts_scheme(line, target_open + 1) {
            continue;
        }
        let target_start = target_open + 1;
        let Some(target_end) = line[target_start..]
            .iter()
            .position(|c| *c == ')')
            .map(|end| end + target_start)
        else {
            continue;
        };
        let Some(label_end) = line[..target_open]
            .iter()
            .rposition(|c| !c.is_whitespace())
            .map(|index| index + 1)
        else {
            continue;
        };
        let label_start = line[..label_end]
            .iter()
            .rposition(|c| c.is_whitespace())
            .map_or(0, |index| index + 1);
        let valid_target = !line[target_start..target_end]
            .iter()
            .any(|c| c.is_whitespace());
        if col >= label_start && col < label_end && valid_target {
            return Some((
                label_start,
                label_end,
                line[target_start..target_end].iter().collect(),
            ));
        }
    }
    None
}

/// Convert a visible `:line[:column]` suffix to a file-URI fragment.
pub fn source_fragment(token: &str) -> Option<String> {
    let mut parts = token.rsplitn(3, ':');
    let last = parts.next()?;
    let prior = parts.next()?;
    if let (Ok(column), Ok(line)) = (last.parse::<u32>(), prior.parse::<u32>()) {
        return Some(format!("#L{line}C{column}"));
    }
    let line = last.parse::<u32>().ok()?;
    Some(format!("#L{line}"))
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
        // Must look like a path: contains a separator or is filename-shaped,
        // isn't a URL (those have "://"), and has a plausible lead-in. Bare
        // names remain candidates only; the caller checks them against cwd.
        let has_slash = token.contains(&'/') || token.contains(&'\\');
        let is_url = token.windows(3).any(|w| w == [':', '/', '/']);
        let good_start = token.first().is_some_and(|c| {
            *c == '/' || *c == '\\' || *c == '~' || *c == '.' || c.is_alphanumeric() || *c == '_'
        });
        let stripped = strip_line_suffix(token);
        let bare_filename = !has_slash
            && stripped.len() >= 3
            && stripped[1..].contains(&'.')
            && stripped.last().is_some_and(|c| c.is_alphanumeric());
        if (has_slash || bare_filename) && !is_url && good_start && token.len() >= 2 {
            let path: String = stripped.iter().collect();
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
    // Refuse UNC / network paths (`\\host\share\…`, `//host/share/…`). Probing
    // one with `exists()` — which link detection does on a mere Ctrl-hover — makes
    // Windows perform an SMB authentication to the named host, a well-known way to
    // capture a user's NetNTLM hash. A malicious repo could print such a string to
    // a build log; it must never become a clickable/probed link.
    if has_network_prefix(raw) {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if raw == "~" {
        return home.map(Path::to_path_buf);
    }
    if raw.starts_with('/') {
        return Some(PathBuf::from(raw));
    }
    // Windows absolute: `D:\…` or `D:/…`.
    if is_windows_drive_abs(raw) {
        return Some(PathBuf::from(raw));
    }
    cwd.map(|c| c.join(raw))
}

fn is_windows_drive_abs(raw: &str) -> bool {
    let b = raw.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
}

fn has_network_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 2 && matches!(bytes[0], b'/' | b'\\') && matches!(bytes[1], b'/' | b'\\')
}

/// A `file://` URI for an absolute path, percent-encoding everything outside
/// the unreserved set + `/` (so spaces etc. survive the trip through xdg-open /
/// ShellExecute). On Windows, drive paths become `file:///D:/…` with forward
/// slashes — the form Windows handlers accept (not `file://D%3A%5C…`).
pub fn file_uri(path: &Path) -> String {
    let mut s = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        s = s.replace('\\', "/");
    }
    // `file://` + absolute path: Unix `/tmp/x` → `file:///tmp/x`;
    // Windows `D:/x` → `file:///D:/x` (need the extra slash).
    let mut uri = String::from("file://");
    if cfg!(windows) && !s.starts_with('/') {
        uri.push('/');
    }
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                uri.push(b as char);
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

/// Decode a `file://` URI back to a filesystem path, or `None` if `uri` is not
/// a file URL. Handles `file:///tmp/x`, `file:///D:/x`, and percent-encoding.
pub fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    file_target_from_uri(uri).map(|target| target.path)
}

pub fn file_target_from_uri(uri: &str) -> Option<FileLinkTarget> {
    let rest = uri.strip_prefix("file://")?;
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    // `file:///path` → path starts with `/`; `file://localhost/path` rare, skip.
    let path_part = if rest.len() >= "localhost".len()
        && rest[.."localhost".len()].eq_ignore_ascii_case("localhost")
        && rest.as_bytes().get("localhost".len()) == Some(&b'/')
    {
        &rest["localhost".len()..]
    } else if rest.starts_with('/') {
        rest
    } else {
        return None;
    };
    // Percent-decode.
    let mut out = Vec::with_capacity(path_part.len());
    let bytes = path_part.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let v = u8::from_str_radix(h, 16).ok()?;
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    if decoded.is_empty() {
        return None;
    }
    // Never turn a URI into a UNC/network path. Link validation calls
    // `exists()` on hover; probing an attacker-controlled SMB host can leak
    // Windows credentials. Check after percent-decoding to block smuggling.
    if has_network_prefix(&decoded) {
        return None;
    }
    // Unix: `/tmp/x`. Windows: `/D:/x` or `/D|/x` (some emitters) → `D:/x`.
    #[cfg(windows)]
    {
        let trimmed = decoded
            .strip_prefix('/')
            .filter(|s| {
                let b = s.as_bytes();
                b.len() >= 2 && b[0].is_ascii_alphabetic() && (b[1] == b':' || b[1] == b'|')
            })
            .map(|s| {
                if s.as_bytes()[1] == b'|' {
                    format!("{}:{}", &s[..1], &s[2..])
                } else {
                    s.to_string()
                }
            })
            .unwrap_or(decoded);
        Some(FileLinkTarget {
            path: PathBuf::from(trimmed),
            line: parse_source_fragment(fragment).map(|p| p.0),
            column: parse_source_fragment(fragment).and_then(|p| p.1),
        })
    }
    #[cfg(not(windows))]
    {
        Some(FileLinkTarget {
            path: PathBuf::from(decoded),
            line: parse_source_fragment(fragment).map(|p| p.0),
            column: parse_source_fragment(fragment).and_then(|p| p.1),
        })
    }
}

fn parse_source_fragment(fragment: &str) -> Option<(u32, Option<u32>)> {
    let source = fragment.strip_prefix('L')?;
    let (line, column) = source
        .split_once('C')
        .map(|(line, column)| (line, Some(column)))
        .unwrap_or((source, None));
    let line = line.parse().ok()?;
    let column = column.and_then(|column| column.parse().ok());
    Some((line, column))
}

#[cfg(test)]
mod tests {
    use super::{
        file_target_from_uri, file_uri, hard_wraps_to_next, markdown_link_at, markdown_target_at,
        path_from_file_uri, path_span_at, path_spans, rendered_markdown_link_at, resolve_path,
        source_fragment, stitch_rows, strip_box_margins, url_span_at, url_spans,
    };
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
    fn finds_plain_file_uri() {
        let line = chars("open file:///D:/dev/muxel/README.md");
        assert_eq!(
            find_url_at(&line, 10).as_deref(),
            Some("file:///D:/dev/muxel/README.md")
        );
    }

    #[test]
    fn markdown_label_carries_target() {
        let line = chars("[browser.rs:112](file:///D:/dev/muxel/browser.rs#L112)");
        assert_eq!(
            markdown_link_at(&line, 5),
            Some((1, 15, "file:///D:/dev/muxel/browser.rs#L112".to_string()))
        );
        assert!(markdown_link_at(&line, 20).is_none());
        assert_eq!(
            markdown_target_at(&line, 25),
            Some((17, 53, "file:///D:/dev/muxel/browser.rs#L112".to_string()))
        );
    }

    #[test]
    fn malformed_markup_does_not_hide_a_later_valid_link() {
        let literal = chars("[broken [good](https://example.com)");
        assert_eq!(
            markdown_link_at(&literal, 10).map(|(_, _, target)| target),
            Some("https://example.com".to_string())
        );
        let rendered = chars("bad (https://broken good (https://example.com)");
        assert_eq!(
            rendered_markdown_link_at(&rendered, 21).map(|(_, _, target)| target),
            Some("https://example.com".to_string())
        );
        let rendered = chars("(https://ignored) good (https://example.com)");
        assert_eq!(
            rendered_markdown_link_at(&rendered, 19).map(|(_, _, target)| target),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn two_urls_on_one_line() {
        let line = chars("https://a.com/1 and https://b.com/2");
        assert_eq!(url_spans(&line).len(), 2);
        assert_eq!(find_url_at(&line, 25).as_deref(), Some("https://b.com/2"));
    }

    #[test]
    fn stitches_claude_hanging_indent_url_without_margin_spaces() {
        let rows = vec![
            (chars("  Published https://claude.ai/c      "), false),
            (chars("    ode/artifact/dd8e7386-edf1     "), false),
            (chars("    -4ead-915a-1272481f3b7c        "), false),
        ];
        let stitched = stitch_rows(&rows, 0, 20);
        assert_eq!(
            url_span_at(&stitched.chars, stitched.clicked_col)
                .map(|(_, _, url)| url)
                .as_deref(),
            Some("https://claude.ai/code/artifact/dd8e7386-edf1-4ead-915a-1272481f3b7c")
        );
    }

    #[test]
    fn stitches_claude_hanging_indent_file_uri() {
        let rows = vec![
            (chars("file:///D:/temp/windows/claude/D--dev-   "), false),
            (chars("    moxie/a7962661/scratchpad/off-by-   "), false),
            (chars("    one.html                            "), false),
        ];
        let stitched = stitch_rows(&rows, 1, 12);
        assert_eq!(
            url_span_at(&stitched.chars, stitched.clicked_col)
                .map(|(_, _, url)| url)
                .as_deref(),
            Some("file:///D:/temp/windows/claude/D--dev-moxie/a7962661/scratchpad/off-by-one.html")
        );
    }

    #[test]
    fn grok_rendered_markdown_label_carries_wrapped_file_uri() {
        let rows = vec![
            (chars("joke.html (file:///D:/       "), false),
            (chars("dev/moxie/joke.html)         "), false),
        ];
        let stitched = stitch_rows(&rows, 0, 3);
        assert_eq!(
            rendered_markdown_link_at(&stitched.chars, stitched.clicked_col),
            Some((0, 9, "file:///D:/dev/moxie/joke.html".to_string()))
        );
        let target_click = stitch_rows(&rows, 1, 8);
        assert_eq!(
            url_span_at(&target_click.chars, target_click.clicked_col)
                .map(|(_, _, url)| url)
                .as_deref(),
            Some("file:///D:/dev/moxie/joke.html")
        );
    }

    #[test]
    fn grok_inset_file_uri_rows_are_hard_wrap_candidates() {
        let mut opening = chars("Draft at issue.md (file:///");
        opening.resize(100, ' ');
        assert!(hard_wraps_to_next(&opening, 0));

        let mut continuation = chars("D:/dev/orez-desk/issues/routing-pack.md");
        continuation.resize(100, ' ');
        assert!(hard_wraps_to_next(&continuation, 0));

        let mut ordinary_url = chars("See https://example.com/complete");
        ordinary_url.resize(100, ' ');
        assert!(!hard_wraps_to_next(&ordinary_url, 0));
    }

    #[test]
    fn grok_wide_inset_file_uri_rows_are_hard_wrap_candidates() {
        let mut opening = chars("                [joke.html](file:///");
        opening.resize(80, ' ');
        assert!(hard_wraps_to_next(&opening, 16));

        let mut continuation = chars("                D:/dev/moxie/");
        continuation.resize(80, ' ');
        assert!(hard_wraps_to_next(&continuation, 16));
    }

    #[test]
    fn ordinary_prose_ending_in_a_path_is_not_joined() {
        let mut row = chars("Compiled src/main.rs");
        row.resize(120, ' ');
        assert!(!hard_wraps_to_next(&row, 0));
    }

    #[test]
    fn parenthesized_file_uri_can_continue_after_a_directory_fragment() {
        let mut row = chars("Draft at joke.html (file:///D:/dev/");
        row.resize(120, ' ');
        assert!(hard_wraps_to_next(&row, 0));
    }

    #[test]
    fn markdown_destinations_cannot_absorb_unrelated_prose() {
        let line = chars("[WARN](https://example.com is down)");
        assert!(markdown_link_at(&line, 2).is_none());
        assert!(markdown_target_at(&line, 10).is_none());
    }

    #[test]
    fn literal_markdown_target_survives_wide_inset_hard_wraps() {
        let rows = vec![
            (
                chars("                [joke.html](file:///          "),
                false,
            ),
            (
                chars("                D:/dev/moxie/                 "),
                false,
            ),
            (
                chars("                joke.html#L42)                "),
                false,
            ),
        ];
        let stitched = stitch_rows(&rows, 1, 20);
        assert_eq!(
            markdown_target_at(&stitched.chars, stitched.clicked_col)
                .map(|(_, _, target)| target)
                .as_deref(),
            Some("file:///D:/dev/moxie/joke.html#L42")
        );
    }

    #[test]
    fn box_margins_do_not_split_wrapped_markdown_targets() {
        let mut rows = vec![
            chars(" │   [joke.html](file:///              │"),
            chars(" │   D:/dev/moxie/                     │"),
            chars(" │   joke.html)                        │"),
        ];
        for row in &mut rows {
            strip_box_margins(row);
        }
        let rows: Vec<_> = rows.into_iter().map(|row| (row, false)).collect();
        let stitched = stitch_rows(&rows, 1, 8);
        assert_eq!(
            markdown_target_at(&stitched.chars, stitched.clicked_col)
                .map(|(_, _, target)| target)
                .as_deref(),
            Some("file:///D:/dev/moxie/joke.html")
        );
    }

    #[test]
    fn internal_table_separators_are_not_treated_as_box_margins() {
        let mut row = chars("column one │ column two │ column three");
        let original = row.clone();
        strip_box_margins(&mut row);
        assert_eq!(row, original);
    }

    #[test]
    fn hanging_indent_is_not_part_of_the_wrapped_link() {
        let rows = vec![
            (chars("file:///D:/dev/moxie/       "), false),
            (chars("    joke.html               "), false),
        ];
        let indent = stitch_rows(&rows, 1, 2);
        assert!(!indent.clicked_in_content);
        let link = stitch_rows(&rows, 1, 6);
        assert!(link.clicked_in_content);
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
    fn finds_bare_filename_for_cwd_resolution() {
        let line = chars("opened joke.html but not documentation");
        assert_eq!(
            path_span_at(&line, 9),
            Some((7, 16, "joke.html".to_string()))
        );
        assert!(path_span_at(&line, 25).is_none());
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
        assert_eq!(
            source_fragment("src/main.rs:42:7").as_deref(),
            Some("#L42C7")
        );
        assert_eq!(source_fragment("src/main.rs:42").as_deref(), Some("#L42"));
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
    fn resolve_path_refuses_unc_network_paths() {
        let cwd = Path::new("/work/proj");
        let home = Path::new("/home/u");
        // UNC / network paths must not resolve — an exists() probe on Windows
        // would trigger SMB auth to the host (NetNTLM-hash capture vector).
        assert_eq!(
            resolve_path(r"\\attacker.example.com\share\x", Some(cwd), Some(home)),
            None
        );
        assert_eq!(
            resolve_path("//attacker.example.com/share/x", Some(cwd), Some(home)),
            None
        );
        assert_eq!(
            resolve_path(r"/\attacker.example.com\share\x", Some(cwd), Some(home)),
            None
        );
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

    #[test]
    #[cfg(windows)]
    fn file_uri_windows_drive_path() {
        let uri = file_uri(Path::new(r"D:\dev\proj\.wip\review.md"));
        assert_eq!(uri, "file:///D:/dev/proj/.wip/review.md");
        assert_eq!(
            path_from_file_uri(&uri).as_deref(),
            Some(Path::new(r"D:\dev\proj\.wip\review.md"))
        );
    }

    #[test]
    fn path_from_file_uri_unix() {
        assert_eq!(
            path_from_file_uri("file:///tmp/a%20b/c.rs").as_deref(),
            Some(Path::new("/tmp/a b/c.rs"))
        );
        assert_eq!(
            path_from_file_uri("file:///tmp/a.rs#L12C4").as_deref(),
            Some(Path::new("/tmp/a.rs"))
        );
        let target = file_target_from_uri("file:///tmp/a.rs#L12C4").unwrap();
        assert_eq!(target.line, Some(12));
        assert_eq!(target.column, Some(4));
        assert_eq!(
            path_from_file_uri("file://localhost/tmp/a.rs").as_deref(),
            Some(Path::new("/tmp/a.rs"))
        );
        assert_eq!(
            path_from_file_uri("file://LOCALHOST/tmp/a.rs").as_deref(),
            Some(Path::new("/tmp/a.rs"))
        );
        assert!(path_from_file_uri("file://localhost.evil/tmp/a.rs").is_none());
        assert!(path_from_file_uri("file://server/share/a.rs").is_none());
        assert!(path_from_file_uri("file:////server/share/a.rs").is_none());
        assert!(path_from_file_uri("file:///%2F%2Fserver/share/a.rs").is_none());
        assert!(path_from_file_uri("file:///%5Cserver/share/a.rs").is_none());
    }

    #[test]
    fn resolve_windows_drive_abs() {
        assert_eq!(
            resolve_path(r"D:\dev\foo.rs", None, None),
            Some(PathBuf::from(r"D:\dev\foo.rs"))
        );
        assert_eq!(
            resolve_path("D:/dev/foo.rs", None, None),
            Some(PathBuf::from("D:/dev/foo.rs"))
        );
    }

    #[test]
    fn resolve_dot_relative() {
        let cwd = Path::new("/work/proj");
        assert_eq!(
            resolve_path(".wip/review.md", Some(cwd), None),
            Some(PathBuf::from("/work/proj/.wip/review.md"))
        );
    }
}
